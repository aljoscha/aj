//! A bounded thread-safe ring buffer with blocking and non-blocking access.
//!
//! [`Queue`] is a fixed-capacity FIFO shared between producer and consumer
//! threads. It is a `Mutex` guarding the ring state plus two `Condvar`s,
//! `not_full` and `not_empty`, woken on the full->non-full and
//! empty->non-empty transitions. Blocking `push`/`pop` re-check their
//! condition in a `while` loop (via `Condvar::wait_while`) so a spurious
//! wakeup, or a wakeup that races another thread into the slot, sends the
//! waiter back to sleep instead of operating on a full or empty buffer.
//!
//! ## Two-mirror indices
//!
//! `read_index` and `write_index` live in `[0, 2 * SIZE)` rather than
//! `[0, SIZE)`. The slot for an index is `index % SIZE`, while the index
//! itself advances modulo `2 * SIZE`. This extra bit distinguishes "empty"
//! (`write == read`) from "full" (`write` is exactly `SIZE` ahead of `read`,
//! i.e. `(write + SIZE) % (2 * SIZE) == read`) without a separate count, which
//! a plain `[0, SIZE)` scheme cannot do because there both states collapse to
//! `write == read`.
//!
//! ## Lock-held helpers
//!
//! The index arithmetic and slot moves live on [`Inner`], the guarded state,
//! so they are only reachable through a held `MutexGuard`. "Lock held" is thus
//! enforced by the type rather than by convention. The signalling helpers live
//! on [`Queue`] instead, because firing a condvar needs both the guarded state
//! (to read the transition) and the sibling `Condvar` fields.
//!
//! ## Poisoning
//!
//! The std primitives are infallible except on a poisoned mutex, which only
//! happens if a thread panics while holding the lock. The critical sections
//! here are pure index math and a single slot move, none of which panic, so a
//! poisoned queue mutex is a bug in this module. We surface the ergonomic
//! `T` / `Option<T>` signatures and treat poisoning as unreachable by panicking
//! with a clear message rather than threading a `Result` through every caller.

use std::sync::{Condvar, Mutex, MutexGuard};

const POISON_MSG: &str = "queue mutex poisoned: a thread panicked while holding the queue lock";

/// Ring state guarded by the queue mutex.
///
/// All methods assume the queue lock is held, which the type guarantees: an
/// `Inner` is only reachable through the `MutexGuard` handed out by the lock.
struct Inner<T, const SIZE: usize> {
    buf: [Option<T>; SIZE],
    read_index: usize,
    write_index: usize,
}

impl<T, const SIZE: usize> Inner<T, SIZE> {
    fn is_empty(&self) -> bool {
        self.write_index == self.read_index
    }

    fn is_full(&self) -> bool {
        self.mask2(self.write_index + self.buf.len()) == self.read_index
    }

    /// Number of queued items, correcting for the write index having wrapped
    /// below the read index.
    //
    // Part of the two-mirror index surface but not needed by the queue
    // operations themselves, which compare indices directly. Kept and tested
    // so the wrap-correction math has a home.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        let wrap_offset = if self.write_index < self.read_index {
            2 * self.buf.len()
        } else {
            0
        };
        (self.write_index + wrap_offset) - self.read_index
    }

    /// Slot for `index`: `index` modulo the backing length.
    fn mask(&self, index: usize) -> usize {
        index % self.buf.len()
    }

    /// Advances `index` modulo twice the backing length, keeping it in the
    /// `[0, 2 * SIZE)` range the two-mirror scheme relies on.
    fn mask2(&self, index: usize) -> usize {
        index % (2 * self.buf.len())
    }

    /// Writes `item` into the next free slot and advances the write index.
    /// Caller must ensure the queue is not full.
    fn push_raw(&mut self, item: T) {
        let slot = self.mask(self.write_index);
        self.buf[slot] = Some(item);
        self.write_index = self.mask2(self.write_index + 1);
    }

    /// Removes and returns the oldest item, advancing the read index. Caller
    /// must ensure the queue is not empty.
    fn pop_raw(&mut self) -> T {
        let slot = self.mask(self.read_index);
        // The two-mirror invariant keeps every index in `[read, write)`
        // mapped to a `Some`, and callers only pop when non-empty, so the
        // slot is occupied.
        let item = self.buf[slot].take().expect("popped an empty slot");
        self.read_index = self.mask2(self.read_index + 1);
        item
    }
}

/// Thread safe, fixed size, with blocking `push` and `pop`.
///
/// `SIZE` is the capacity in items. Cloneable handles are obtained by wrapping
/// the queue in an [`std::sync::Arc`] and sharing it across threads.
pub struct Queue<T, const SIZE: usize> {
    inner: Mutex<Inner<T, SIZE>>,
    /// Woken when the queue transitions full->non-full (a producer may proceed).
    not_full: Condvar,
    /// Woken when the queue transitions empty->non-empty (a consumer may proceed).
    not_empty: Condvar,
}

impl<T, const SIZE: usize> Default for Queue<T, SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const SIZE: usize> Queue<T, SIZE> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                // No `T: Default`/`Copy` bound: each slot starts empty.
                buf: std::array::from_fn(|_| None),
                read_index: 0,
                write_index: 0,
            }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        }
    }

    /// Pops the oldest item, blocking until one is available.
    pub fn pop(&self) -> T {
        let inner = self.lock_inner();
        let mut inner = self
            .not_empty
            .wait_while(inner, |inner| inner.is_empty())
            .expect(POISON_MSG);
        debug_assert!(!inner.is_empty());
        self.pop_and_signal(&mut inner)
    }

    /// Pushes `item`, blocking until there is room.
    pub fn push(&self, item: T) {
        let inner = self.lock_inner();
        let mut inner = self
            .not_full
            .wait_while(inner, |inner| inner.is_full())
            .expect(POISON_MSG);
        debug_assert!(!inner.is_full());
        self.push_and_signal(&mut inner, item);
    }

    /// Pushes `item` without blocking. Returns `false` if the queue is full.
    pub fn try_push(&self, item: T) -> bool {
        let mut inner = self.lock_inner();
        if inner.is_full() {
            return false;
        }
        self.push_and_signal(&mut inner, item);
        true
    }

    /// Pops the oldest item without blocking. Returns `None` if the queue is
    /// empty.
    pub fn try_pop(&self) -> Option<T> {
        let mut inner = self.lock_inner();
        if inner.is_empty() {
            return None;
        }
        Some(self.pop_and_signal(&mut inner))
    }

    /// Blocks until the queue is non-empty without removing anything.
    ///
    /// Useful to wait for work and then drain under an external lock (see
    /// [`Queue::lock`]) so the check-then-drain happens atomically.
    pub fn poll(&self) {
        let inner = self.lock_inner();
        let _guard = self
            .not_empty
            .wait_while(inner, |inner| inner.is_empty())
            .expect(POISON_MSG);
        // The lock releases when `_guard` drops. We return having observed a
        // non-empty queue but leave the items in place for the caller.
    }

    /// Returns `true` if the queue is currently empty.
    pub fn is_empty(&self) -> bool {
        self.lock_inner().is_empty()
    }

    /// Returns `true` if the queue is currently full.
    pub fn is_full(&self) -> bool {
        self.lock_inner().is_full()
    }

    /// Takes the queue lock and returns a guard for draining under an
    /// externally held lock.
    ///
    /// NOTE: This replaces upstream's manual `lock`/`drain`/`unlock` trio with
    /// a borrow-checked guard. The render loop wants to grab the lock once and
    /// pop every queued item in a tight loop without releasing it between
    /// items. [`QueueGuard::drain`] does exactly that, and the lock is released
    /// when the guard drops, so the "lock externally, drain in a loop" pattern
    /// survives while the unbalanced lock/unlock pair cannot be misused.
    pub fn lock(&self) -> QueueGuard<'_, T, SIZE> {
        QueueGuard {
            queue: self,
            inner: self.lock_inner(),
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner<T, SIZE>> {
        self.inner.lock().expect(POISON_MSG)
    }

    /// Pushes under the held lock, waking one `not_empty` waiter on the
    /// empty->non-empty transition.
    fn push_and_signal(&self, inner: &mut Inner<T, SIZE>, item: T) {
        let was_empty = inner.is_empty();
        inner.push_raw(item);
        if was_empty {
            self.not_empty.notify_one();
        }
    }

    /// Pops under the held lock, waking one `not_full` waiter on the
    /// full->non-full transition.
    fn pop_and_signal(&self, inner: &mut Inner<T, SIZE>) -> T {
        let was_full = inner.is_full();
        let item = inner.pop_raw();
        if was_full {
            self.not_full.notify_one();
        }
        item
    }
}

/// A held queue lock for draining items in a loop.
///
/// Holds the queue's `MutexGuard` for its whole lifetime, so no producer or
/// consumer can touch the queue until it drops. Created by [`Queue::lock`].
pub struct QueueGuard<'a, T, const SIZE: usize> {
    queue: &'a Queue<T, SIZE>,
    inner: MutexGuard<'a, Inner<T, SIZE>>,
}

impl<T, const SIZE: usize> QueueGuard<'_, T, SIZE> {
    /// Pops the oldest item under the held lock, or `None` if empty.
    ///
    /// Re-signals `not_full` on a full->non-full transition just like a normal
    /// `pop`, so a producer blocked on a full queue is woken once this guard
    /// releases the lock.
    pub fn drain(&mut self) -> Option<T> {
        if self.inner.is_empty() {
            return None;
        }
        Some(self.queue.pop_and_signal(&mut self.inner))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn simple_push_pop() {
        let queue = Queue::<u8, 16>::new();
        queue.push(1);
        queue.push(2);
        assert_eq!(queue.pop(), 1);
        assert_eq!(queue.pop(), 2);
    }

    #[test]
    fn fill_wait_to_push_pop_in_another_thread() {
        let queue = Arc::new(Queue::<u8, 2>::new());
        queue.push(1);
        queue.push(2);

        let task = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                queue.push(3);
                assert_eq!(queue.pop(), 2);
            })
        };

        assert!(!queue.try_push(3));
        assert_eq!(queue.pop(), 1);
        task.join().unwrap();
        assert_eq!(queue.pop(), 3);
        assert_eq!(queue.try_pop(), None);
    }

    #[test]
    fn try_to_pop_fill_from_another_thread() {
        let queue = Arc::new(Queue::<u8, 2>::new());

        let task = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                for i in 0..5u8 {
                    queue.push(i);
                }
            })
        };

        for idx in 0..5u8 {
            assert_eq!(queue.pop(), idx);
        }
        task.join().unwrap();
    }

    /// Background thread for [`fill_block_fill_block`]. It fires spurious
    /// condvar signals and sleeps before actually making room, so the blocked
    /// push must re-check the full queue and go back to sleep.
    fn sleepy_pop(q: &Queue<u8, 2>, state: &AtomicU8) {
        // Wait until the main thread has filled the queue.
        while state.load(Ordering::Acquire) < 1 {
            thread::yield_now();
        }

        // Spurious wake: the queue is still full, so a correct push ignores it.
        q.not_full.notify_one();
        q.not_empty.notify_one();

        // Give the other thread a chance to wake, see it is still full, and go
        // back to sleep. yield_now alone does not guarantee scheduling, so we
        // also sleep.
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));
        // Now actually make room, unblocking the push.
        assert_eq!(q.pop(), 1);

        // Wait for the main thread to signal it is about to push again.
        while state.load(Ordering::Acquire) < 2 {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(10));

        // Another spurious wake, and another chance to go back to sleep.
        q.not_full.notify_one();
        q.not_empty.notify_one();
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));

        assert_eq!(q.pop(), 2);
    }

    #[test]
    fn fill_block_fill_block() {
        // Fill the queue, block trying to write another item, have a
        // background thread unblock us, then block writing yet another. This
        // fails if the `while` loop in `push` becomes an `if`: the spurious
        // signal would let the push proceed into a still-full buffer.
        let queue = Arc::new(Queue::<u8, 2>::new());
        let state = Arc::new(AtomicU8::new(0));

        let task = {
            let queue = Arc::clone(&queue);
            let state = Arc::clone(&state);
            thread::spawn(move || sleepy_pop(&queue, &state))
        };

        queue.push(1);
        queue.push(2);
        state.store(1, Ordering::Release);
        let now = Instant::now();
        queue.push(3); // This one should block.
        let elapsed = now.elapsed();

        // Confirm the background sleeps yielded to this thread: the push
        // blocked for longer than 5 ms instead of slipping through a spurious
        // wake.
        assert!(elapsed > Duration::from_millis(5));

        state.store(2, Ordering::Release);
        queue.push(4); // Blocks again, waiting for the other thread.

        task.join().unwrap();
        assert_eq!(queue.pop(), 3);
        assert_eq!(queue.pop(), 4);
    }

    /// Background thread for [`drain_block_drain_block`]. Mirror of
    /// [`sleepy_pop`] on the producer side.
    fn sleepy_push(q: &Queue<u8, 1>, state: &AtomicU8) {
        // Try to ensure the other thread is already blocked on an empty pop.
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));

        // Spurious wake: the queue is still empty, so a correct pop ignores it.
        q.not_full.notify_one();
        q.not_empty.notify_one();

        thread::yield_now();
        thread::sleep(Duration::from_millis(10));

        // Now actually provide an item to pop.
        q.push(1);
        // Wait until it has been popped and the other thread blocks again.
        while state.load(Ordering::Acquire) < 1 {
            thread::yield_now();
        }
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));

        // Another spurious wake before the real second push.
        q.not_full.notify_one();
        q.not_empty.notify_one();

        q.push(2);
    }

    #[test]
    fn drain_block_drain_block() {
        // The pop-side mirror of fill/block/fill/block. This fails if the
        // `while` loop in `pop` becomes an `if`: a spurious signal would let
        // the pop proceed and take from an empty slot.
        let queue = Arc::new(Queue::<u8, 1>::new());
        let state = Arc::new(AtomicU8::new(0));

        let task = {
            let queue = Arc::clone(&queue);
            let state = Arc::clone(&state);
            thread::spawn(move || sleepy_push(&queue, &state))
        };

        assert_eq!(queue.pop(), 1);
        state.store(1, Ordering::Release);
        assert_eq!(queue.pop(), 2);
        task.join().unwrap();
    }

    #[test]
    fn two_readers() {
        // Two threads read, one thread writes.
        let queue = Arc::new(Queue::<u8, 1>::new());

        let t1 = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || assert_eq!(queue.pop(), 1))
        };
        let t2 = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || assert_eq!(queue.pop(), 1))
        };

        // Give both readers time to block on the empty queue before pushing.
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));
        queue.push(1);
        queue.push(1);
        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn two_writers() {
        let queue = Arc::new(Queue::<u8, 1>::new());

        let t1 = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(1))
        };
        let t2 = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(1))
        };

        assert_eq!(queue.pop(), 1);
        assert_eq!(queue.pop(), 1);
        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn two_mirror_len() {
        let queue = Queue::<u8, 2>::new();
        assert_eq!(queue.lock_inner().len(), 0);

        queue.push(10);
        queue.push(20);
        assert_eq!(queue.lock_inner().len(), 2);

        // Advance read and write indices until the write index wraps below the
        // read index, exercising the 2*SIZE correction in `len`.
        assert_eq!(queue.pop(), 10);
        queue.push(30);
        assert_eq!(queue.pop(), 20);
        queue.push(40); // write index now wraps below read index
        assert_eq!(queue.lock_inner().len(), 2);
        assert!(queue.lock_inner().is_full());

        assert_eq!(queue.pop(), 30);
        assert_eq!(queue.pop(), 40);
        assert_eq!(queue.lock_inner().len(), 0);
    }

    #[test]
    fn drain_under_external_lock() {
        let queue = Queue::<u8, 2>::new();
        queue.push(1);
        queue.push(2);

        let mut guard = queue.lock();
        assert_eq!(guard.drain(), Some(1));
        assert_eq!(guard.drain(), Some(2));
        assert_eq!(guard.drain(), None);
        drop(guard);

        assert!(queue.is_empty());
    }

    #[test]
    fn drain_wakes_blocked_producer() {
        let queue = Arc::new(Queue::<u8, 1>::new());
        queue.push(1); // full

        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(2))
        };

        // Give the producer a chance to block on the full queue.
        thread::yield_now();
        thread::sleep(Duration::from_millis(10));
        {
            let mut guard = queue.lock();
            // Draining a full queue re-signals `not_full`, so the blocked
            // producer proceeds once we release the lock here.
            assert_eq!(guard.drain(), Some(1));
            assert_eq!(guard.drain(), None);
        }

        producer.join().unwrap();
        assert_eq!(queue.pop(), 2);
    }
}
