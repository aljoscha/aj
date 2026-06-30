//! The posix `/dev/tty` backend: raw mode, window size, SIGWINCH, and panic
//! recovery.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};

use nix::sys::termios::{
    ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios, tcgetattr, tcsetattr,
};

use crate::Winsize;
use crate::ctlseqs;
use crate::tty::{HandlerId, ResizeHandler, Tty};

// TIOCGWINSZ / TIOCSWINSZ are "bad" ioctls: the request code is a fixed
// constant rather than something we compute from the data type, so we use the
// `_bad` macro variants. Both wrap `ioctl(2)` and are `unsafe` because they
// move data through a raw pointer we supply.
nix::ioctl_read_bad!(tiocgwinsz, nix::libc::TIOCGWINSZ, nix::libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, nix::libc::TIOCSWINSZ, nix::libc::winsize);

/// A terminal opened on `/dev/tty`, held in raw mode for its lifetime.
///
/// We hold a single open description of the tty, matching upstream which opens
/// `/dev/tty` once read-write. The `File` inside the [`BufWriter`] owns it.
/// Reads, ioctls, and termios calls go through the same fd directly via the
/// stored [`RawFd`]. Reads and writes on a tty are independent, so sharing one
/// description for a buffered write path and an unbuffered read path is correct.
///
/// On drop the original termios is restored (`tcsetattr` with `TCSAFLUSH`) and
/// the global recovery slot is cleared.
pub struct PosixTty {
    /// Buffered writer over the tty. Owns the underlying `File`/fd.
    writer: BufWriter<File>,
    /// The terminal state captured before [`make_raw`], restored on drop.
    original_termios: Termios,
    /// The raw fd shared with `writer`, used for reads, ioctls, and termios.
    /// Valid until `writer` (and its `File`) is dropped. Our explicit `Drop`
    /// runs before field drops, so the termios restore there still sees it.
    fd: RawFd,
}

impl PosixTty {
    /// Opens `/dev/tty`, enters raw mode, and installs the SIGWINCH handler and
    /// the panic-recovery hook.
    ///
    /// This is the production entry point. The SIGWINCH and panic-hook
    /// installation are process-global side effects, so they live here and not
    /// in [`Self::from_fd`], which tests use to drive the backend over a PTY.
    pub fn new() -> io::Result<Self> {
        install_signal_handler()?;
        install_panic_hook();
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        Self::from_fd(OwnedFd::from(file))
    }

    /// Constructs a `PosixTty` from an already-open terminal fd.
    ///
    /// This is the test seam. CI may have no controlling terminal, so tests
    /// pass the slave fd of a `nix::pty::openpty` pair here. Unlike
    /// [`Self::new`] it does not install the SIGWINCH handler or the panic hook
    /// (those are process-global), but it does register the fd in the global
    /// recovery slot so the [`recover`] path can be exercised.
    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let original = tcgetattr(&fd)?;
        let mut raw = original.clone();
        make_raw(&mut raw);
        tcsetattr(&fd, SetArg::TCSAFLUSH, &raw)?;

        let file = File::from(fd);
        let raw_fd = file.as_raw_fd();
        set_global_tty(raw_fd, original.clone());

        Ok(Self {
            writer: BufWriter::new(file),
            original_termios: original,
            fd: raw_fd,
        })
    }
}

impl Tty for PosixTty {
    fn writer(&mut self) -> &mut dyn Write {
        &mut self.writer
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(nix::unistd::read(self.fd, buf)?)
    }

    fn get_winsize(&self) -> io::Result<Winsize> {
        let mut ws = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `self.fd` is an open tty for the lifetime of `self`, and `ws`
        // is a valid, correctly-sized out parameter for the ioctl.
        unsafe { tiocgwinsz(self.fd, &mut ws)? };
        Ok(Winsize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            x_pixel: ws.ws_xpixel,
            y_pixel: ws.ws_ypixel,
        })
    }

    fn notify_winsize(&self, handler: ResizeHandler) -> io::Result<HandlerId> {
        register_handler(handler)
    }

    fn remove_winsize(&self, id: HandlerId) {
        unregister_handler(id);
    }
}

impl Drop for PosixTty {
    fn drop(&mut self) {
        // Restore the terminal before the `File` closes the fd. Our explicit
        // Drop runs before field drops, so `self.fd` is still open here.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        if let Err(err) = tcsetattr(borrowed, SetArg::TCSAFLUSH, &self.original_termios) {
            // We are tearing down. Log to stderr rather than panic, since a
            // panic while unwinding would abort the process.
            eprintln!("vaxis: couldn't restore terminal: {err}");
        }
        clear_global_tty(self.fd);
    }
}

/// Applies the raw-mode flag mutations to `termios` in place.
///
/// Reproduces the canonical raw-mode setup (see termios(3)): disable input
/// translation and flow control, output post-processing, echo and line
/// editing, and signal generation, force 8-bit characters with no parity, and
/// make `read` return as soon as one byte is available. Factored out so the
/// exact flag set is unit-testable on a `Termios` obtained from a PTY.
pub(crate) fn make_raw(termios: &mut Termios) {
    termios.input_flags.remove(
        InputFlags::IGNBRK
            | InputFlags::BRKINT
            | InputFlags::PARMRK
            | InputFlags::ISTRIP
            | InputFlags::INLCR
            | InputFlags::IGNCR
            | InputFlags::ICRNL
            | InputFlags::IXON,
    );
    termios.output_flags.remove(OutputFlags::OPOST);
    termios.local_flags.remove(
        LocalFlags::ECHO
            | LocalFlags::ECHONL
            | LocalFlags::ICANON
            | LocalFlags::ISIG
            | LocalFlags::IEXTEN,
    );
    termios.control_flags.remove(ControlFlags::CSIZE);
    termios.control_flags.insert(ControlFlags::CS8);
    termios.control_flags.remove(ControlFlags::PARENB);
    // `libc::VMIN`/`VTIME` are `usize` indices, which sidesteps an enum-to-index
    // cast (the workspace denies `as` conversions).
    termios.control_chars[nix::libc::VMIN] = 1;
    termios.control_chars[nix::libc::VTIME] = 0;
}

// --- SIGWINCH handler registry ---------------------------------------------
//
// Upstream keeps a fixed `[8]SignalHandler` array behind a mutex and invokes
// the callbacks from inside the SIGWINCH handler. We keep the same process-
// global registry but invoke callbacks from a normal thread fed by
// `signal-hook` (D7), so nothing runs in async-signal-unsafe context.

/// Matches upstream's fixed `[8]SignalHandler` capacity.
const MAX_HANDLERS: usize = 8;

struct Registered {
    id: HandlerId,
    callback: ResizeHandler,
}

static REGISTRY: Mutex<Vec<Registered>> = Mutex::new(Vec::new());
static NEXT_HANDLER_ID: AtomicU64 = AtomicU64::new(0);

fn register_handler(callback: ResizeHandler) -> io::Result<HandlerId> {
    let mut reg = REGISTRY.lock().expect("SIGWINCH registry poisoned");
    if reg.len() >= MAX_HANDLERS {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "too many SIGWINCH handlers",
        ));
    }
    let id = HandlerId(NEXT_HANDLER_ID.fetch_add(1, Ordering::Relaxed));
    reg.push(Registered { id, callback });
    Ok(id)
}

fn unregister_handler(id: HandlerId) {
    let mut reg = REGISTRY.lock().expect("SIGWINCH registry poisoned");
    reg.retain(|h| h.id != id);
}

/// Invokes every registered resize callback.
///
/// Called from the `signal-hook` iterator thread, never from the OS signal
/// handler. We clone the callbacks out while holding the lock and invoke them
/// unlocked, so a callback that re-enters the registry (to register or remove a
/// handler) cannot deadlock. This is a deliberate departure from upstream,
/// which runs callbacks while holding the lock inside the signal handler.
fn dispatch_winsize() {
    let callbacks: Vec<ResizeHandler> = {
        let reg = REGISTRY.lock().expect("SIGWINCH registry poisoned");
        reg.iter().map(|h| Arc::clone(&h.callback)).collect()
    };
    for cb in callbacks {
        cb();
    }
}

static SIGNAL_THREAD: OnceLock<()> = OnceLock::new();

/// Installs the SIGWINCH handler once per process.
///
/// `signal-hook` registers a handler that only writes to a self-pipe. We own a
/// dedicated thread that blocks on that pipe and runs the registered callbacks
/// for each delivered signal. This is the full delivery mechanism: the loop
/// (phase 7) registers its resize callback via [`PosixTty::notify_winsize`] and
/// is woken here without any work happening in signal context.
fn install_signal_handler() -> io::Result<()> {
    if SIGNAL_THREAD.get().is_some() {
        return Ok(());
    }
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    std::thread::Builder::new()
        .name("vaxis-sigwinch".into())
        .spawn(move || {
            for _ in &mut signals {
                dispatch_winsize();
            }
        })?;
    // If two threads race here the loser's thread keeps running harmlessly, but
    // in practice `new()` is called once during startup.
    let _ = SIGNAL_THREAD.set(());
    Ok(())
}

// --- Global TTY and panic recovery -----------------------------------------
//
// Upstream stashes a copy of the tty in `global_tty` so a panic handler can
// reset the terminal. We store only what `recover` needs: the fd to write the
// reset bytes to and the original termios to restore. Best-effort, and (as
// upstream notes) it only tracks the most recently created tty.

struct GlobalTty {
    fd: RawFd,
    original_termios: Termios,
}

static GLOBAL_TTY: Mutex<Option<GlobalTty>> = Mutex::new(None);

fn set_global_tty(fd: RawFd, original_termios: Termios) {
    let mut guard = GLOBAL_TTY.lock().expect("global tty poisoned");
    *guard = Some(GlobalTty {
        fd,
        original_termios,
    });
}

fn clear_global_tty(fd: RawFd) {
    let mut guard = GLOBAL_TTY.lock().expect("global tty poisoned");
    // Only clear if we are still the registered tty. A later-created tty may
    // have replaced us, and we must not wipe its registration.
    if guard.as_ref().is_some_and(|g| g.fd == fd) {
        *guard = None;
    }
}

/// Resets the terminal using the global tty slot, best-effort, for use during a
/// panic.
///
/// Writes the reset sequences (pop the kitty keyboard stack, disable mouse
/// reporting, disable bracketed paste, leave the alt screen) directly to the fd
/// and restores the original termios.
///
/// Safety reasoning: this runs from a panic hook, on the panicking thread in
/// ordinary (non-signal) context. Locking a mutex, writing to an fd, and
/// calling `tcsetattr` are all sound here, unlike inside a signal handler. We
/// `try_lock` so that if the panic happened while the slot was already locked
/// we bail quietly instead of deadlocking.
pub fn recover() {
    let Ok(guard) = GLOBAL_TTY.try_lock() else {
        return;
    };
    let Some(global) = guard.as_ref() else {
        return;
    };
    // SAFETY: the panic unwinds destructors only after the hook returns, so the
    // live PosixTty (and thus this fd) is still open while we write.
    let fd = unsafe { BorrowedFd::borrow_raw(global.fd) };
    // We write directly to the fd, bypassing the (inaccessible) BufWriter, so
    // there is no buffer to flush. The four sequences in order are the same
    // bytes upstream concatenates and writes.
    for seq in [
        ctlseqs::CSI_U_POP,
        ctlseqs::MOUSE_RESET,
        ctlseqs::BP_RESET,
        ctlseqs::RMCUP,
    ] {
        let _ = nix::unistd::write(fd, seq.as_bytes());
    }
    let _ = tcsetattr(fd, SetArg::TCSAFLUSH, &global.original_termios);
}

/// Installs the panic hook once per process.
///
/// The hook calls [`recover`] and then chains the previous hook so the default
/// backtrace/abort behavior is preserved.
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            recover();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::sync::atomic::AtomicUsize;

    use nix::pty::openpty;
    use serial_test::serial;

    use super::*;

    /// Opens a PTY for tests, returning `None` (with a logged reason) if the
    /// sandbox has no PTY support, in which case the caller skips.
    fn open_test_pty() -> Option<nix::pty::OpenptyResult> {
        match openpty(None, None) {
            Ok(pty) => Some(pty),
            Err(err) => {
                eprintln!("vaxis: skipping PTY test, openpty failed: {err}");
                None
            }
        }
    }

    #[test]
    fn make_raw_clears_and_sets_expected_flags() {
        let Some(pty) = open_test_pty() else {
            return;
        };
        let original = tcgetattr(pty.slave.as_fd()).expect("tcgetattr");
        let mut raw = original.clone();
        make_raw(&mut raw);

        for flag in [
            InputFlags::IGNBRK,
            InputFlags::BRKINT,
            InputFlags::PARMRK,
            InputFlags::ISTRIP,
            InputFlags::INLCR,
            InputFlags::IGNCR,
            InputFlags::ICRNL,
            InputFlags::IXON,
        ] {
            assert!(
                !raw.input_flags.contains(flag),
                "iflag {flag:?} not cleared"
            );
        }
        assert!(
            !raw.output_flags.contains(OutputFlags::OPOST),
            "OPOST not cleared"
        );
        for flag in [
            LocalFlags::ECHO,
            LocalFlags::ECHONL,
            LocalFlags::ICANON,
            LocalFlags::ISIG,
            LocalFlags::IEXTEN,
        ] {
            assert!(
                !raw.local_flags.contains(flag),
                "lflag {flag:?} not cleared"
            );
        }
        assert_eq!(
            raw.control_flags & ControlFlags::CSIZE,
            ControlFlags::CS8,
            "character size not CS8"
        );
        assert!(
            !raw.control_flags.contains(ControlFlags::PARENB),
            "PARENB not cleared"
        );
        assert_eq!(raw.control_chars[nix::libc::VMIN], 1, "VMIN not 1");
        assert_eq!(raw.control_chars[nix::libc::VTIME], 0, "VTIME not 0");
    }

    #[test]
    #[serial]
    fn raw_mode_round_trip_over_pty() {
        let Some(pty) = open_test_pty() else {
            return;
        };
        // A second handle to the same terminal so we can inspect termios after
        // the PosixTty drops and closes its own fd.
        let probe = pty.slave.try_clone().expect("clone slave");
        let original = tcgetattr(probe.as_fd()).expect("tcgetattr original");
        // A fresh PTY slave starts in cooked mode.
        assert!(original.local_flags.contains(LocalFlags::ICANON));
        assert!(original.local_flags.contains(LocalFlags::ECHO));

        {
            let _tty = PosixTty::from_fd(pty.slave).expect("from_fd");
            let raw = tcgetattr(probe.as_fd()).expect("tcgetattr raw");
            assert!(!raw.local_flags.contains(LocalFlags::ICANON));
            assert!(!raw.local_flags.contains(LocalFlags::ECHO));
            assert!(!raw.local_flags.contains(LocalFlags::ISIG));
            assert!(!raw.input_flags.contains(InputFlags::IXON));
            assert!(!raw.output_flags.contains(OutputFlags::OPOST));
            assert_eq!(raw.control_chars[nix::libc::VMIN], 1);
            assert_eq!(raw.control_chars[nix::libc::VTIME], 0);
        }

        // Drop restored the original termios.
        let restored = tcgetattr(probe.as_fd()).expect("tcgetattr restored");
        assert_eq!(restored.local_flags, original.local_flags);
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.control_flags, original.control_flags);
    }

    #[test]
    #[serial]
    fn get_winsize_reflects_tiocswinsz() {
        let Some(pty) = open_test_pty() else {
            return;
        };
        let probe = pty.slave.try_clone().expect("clone slave");
        let tty = PosixTty::from_fd(pty.slave).expect("from_fd");

        let set = nix::libc::winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 800,
            ws_ypixel: 600,
        };
        // SAFETY: `probe` is an open tty fd and `set` is a valid winsize.
        unsafe { tiocswinsz(probe.as_raw_fd(), &set).expect("tiocswinsz") };

        let ws = tty.get_winsize().expect("get_winsize");
        assert_eq!(ws.rows, 24);
        assert_eq!(ws.cols, 100);
        assert_eq!(ws.x_pixel, 800);
        assert_eq!(ws.y_pixel, 600);
    }

    #[test]
    #[serial]
    fn write_path_reaches_the_terminal() {
        let Some(pty) = open_test_pty() else {
            return;
        };
        let master = pty.master.try_clone().expect("clone master");
        let mut tty = PosixTty::from_fd(pty.slave).expect("from_fd");

        tty.writer().write_all(b"hello").expect("write");
        tty.writer().flush().expect("flush");

        let mut buf = [0u8; 5];
        let mut got = 0;
        while got < buf.len() {
            let n = nix::unistd::read(master.as_raw_fd(), &mut buf[got..]).expect("read master");
            assert!(n > 0, "master read returned EOF");
            got += n;
        }
        assert_eq!(&buf, b"hello");
    }

    #[test]
    #[serial]
    fn winsize_registry_register_invoke_remove() {
        // Exercise the registry and its dispatch without firing a real signal.
        // Start from an empty registry so the count assertions are exact.
        REGISTRY.lock().unwrap().clear();

        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let id = register_handler(Arc::new(move || {
            h.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("register");

        assert_eq!(REGISTRY.lock().unwrap().len(), 1);
        dispatch_winsize();
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        unregister_handler(id);
        assert_eq!(REGISTRY.lock().unwrap().len(), 0);
        dispatch_winsize();
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "removed handler still fired"
        );
    }

    #[test]
    #[serial]
    fn winsize_registry_is_capped() {
        REGISTRY.lock().unwrap().clear();
        let mut ids = Vec::new();
        for _ in 0..MAX_HANDLERS {
            ids.push(register_handler(Arc::new(|| {})).expect("register within cap"));
        }
        let err = register_handler(Arc::new(|| {})).expect_err("over cap should error");
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        for id in ids {
            unregister_handler(id);
        }
    }

    #[test]
    #[serial]
    fn recover_writes_reset_and_restores_termios() {
        let Some(pty) = open_test_pty() else {
            return;
        };
        let master = pty.master.try_clone().expect("clone master");
        let probe = pty.slave.try_clone().expect("clone slave");

        let original = tcgetattr(probe.as_fd()).expect("tcgetattr");
        {
            // While alive, the slave is in raw mode and registered globally.
            let _tty = PosixTty::from_fd(pty.slave).expect("from_fd");
            recover();
            // recover restored termios on the global fd.
            let after = tcgetattr(probe.as_fd()).expect("tcgetattr after recover");
            assert_eq!(after.local_flags, original.local_flags);
        }

        // The master must have received the reset bytes recover wrote.
        let mut buf = vec![0u8; 256];
        let n = nix::unistd::read(master.as_raw_fd(), &mut buf).expect("read master");
        let out = &buf[..n];
        let expected: Vec<u8> = [
            ctlseqs::CSI_U_POP,
            ctlseqs::MOUSE_RESET,
            ctlseqs::BP_RESET,
            ctlseqs::RMCUP,
        ]
        .concat()
        .into_bytes();
        assert!(
            out.windows(expected.len())
                .any(|w| w == expected.as_slice()),
            "reset bytes not found in {out:?}"
        );
    }
}
