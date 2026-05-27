//! System clipboard access for image attachments.
//!
//! Returns `None` whenever the clipboard does not currently hold an
//! image, the read failed, the platform isn't supported, or the data
//! cannot be re-encoded as a supported MIME type. Callers should
//! treat any failure as "user pressed Ctrl+V on a non-image
//! clipboard" — no surfaced error.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use image::{DynamicImage, ImageBuffer, ImageFormat};
use rand::RngCore;

/// Read an image from the system clipboard, encode it as PNG, and
/// write it to a fresh file under [`std::env::temp_dir`]. Returns
/// the absolute path of the written file on success.
///
/// File naming: `aj-clipboard-<random-hex>.png`. The file is NOT
/// auto-deleted — its lifetime must outlive the editor session so a
/// subsequent `read_file` tool call can open it.
pub fn read_image_to_tempfile() -> Option<PathBuf> {
    let dest = temp_dir().join(generate_filename());

    // Primary: arboard. Works on macOS, Windows, and X11 Linux.
    if let Some(png) = read_via_arboard() {
        if fs::write(&dest, &png).is_ok() {
            return Some(dest);
        }
        tracing::debug!("clipboard: failed to write tempfile {}", dest.display());
        return None;
    }

    // Linux fallbacks. arboard's X11 backend doesn't speak Wayland's
    // wl_data_device protocol cleanly for image MIME types, and it
    // has no path to the Windows clipboard from inside WSL.
    if is_wayland()
        && let Some(png) = read_via_wl_paste()
        && fs::write(&dest, &png).is_ok()
    {
        return Some(dest);
    }

    if is_wsl() && read_via_powershell(&dest) {
        return Some(dest);
    }

    None
}

/// Generate a fresh filename of the form `aj-clipboard-<hex>.png`.
/// The hex segment is 16 chars of cryptographic randomness — enough
/// to make accidental collisions in a single `temp_dir` effectively
/// impossible.
fn generate_filename() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("aj-clipboard-{hex}.png")
}

fn temp_dir() -> PathBuf {
    env::temp_dir()
}

fn read_via_arboard() -> Option<Vec<u8>> {
    let mut cb = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(e) => {
            tracing::debug!("clipboard: arboard init failed: {e}");
            return None;
        }
    };
    let img = match cb.get_image() {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!("clipboard: arboard get_image failed: {e}");
            return None;
        }
    };
    let width = u32::try_from(img.width).ok()?;
    let height = u32::try_from(img.height).ok()?;
    let buf: ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, img.bytes.into_owned())?;
    let dynimg = DynamicImage::ImageRgba8(buf);
    let mut out = std::io::Cursor::new(Vec::<u8>::new());
    if let Err(e) = dynimg.write_to(&mut out, ImageFormat::Png) {
        tracing::debug!("clipboard: PNG encode failed: {e}");
        return None;
    }
    Some(out.into_inner())
}

fn is_wayland() -> bool {
    env::var_os("WAYLAND_DISPLAY").is_some()
        || env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

fn is_wsl() -> bool {
    if env::var_os("WSL_DISTRO_NAME").is_some() || env::var_os("WSLENV").is_some() {
        return true;
    }
    fs::read_to_string("/proc/version")
        .map(|s| s.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn read_via_wl_paste() -> Option<Vec<u8>> {
    // `wl-paste --type image/png` writes the clipboard's PNG payload
    // straight to stdout. `--no-newline` keeps it from appending a
    // stray byte to binary data.
    let out = match Command::new("wl-paste")
        .args(["--type", "image/png", "--no-newline"])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("clipboard: wl-paste not installed");
            return None;
        }
        Err(e) => {
            tracing::debug!("clipboard: wl-paste failed: {e}");
            return None;
        }
    };
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

fn read_via_powershell(dest: &Path) -> bool {
    // Translate the Linux destination into a Windows path PowerShell
    // can open from outside WSL.
    let win_path = match Command::new("wslpath").arg("-w").arg(dest).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(_) => return false,
        Err(e) => {
            tracing::debug!("clipboard: wslpath failed: {e}");
            return false;
        }
    };
    if win_path.is_empty() {
        return false;
    }
    // Single-quote the path for PowerShell; escape embedded quotes by
    // doubling per PowerShell's single-quoted-string rules.
    let escaped = win_path.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img) {{ $img.Save('{escaped}', [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' }} \
         else {{ Write-Output 'empty' }}"
    );
    let out = match Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("clipboard: powershell.exe failed: {e}");
            return false;
        }
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.contains("ok") && dest.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_filename_matches_pattern() {
        let name = generate_filename();
        assert!(name.starts_with("aj-clipboard-"), "{name}");
        assert!(name.ends_with(".png"), "{name}");
        let hex = &name["aj-clipboard-".len()..name.len() - ".png".len()];
        assert_eq!(hex.len(), 16);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_filename_is_unique() {
        let a = generate_filename();
        let b = generate_filename();
        assert_ne!(a, b);
    }
}
