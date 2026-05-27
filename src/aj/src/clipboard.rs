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
use std::path::PathBuf;
use std::process::Command;

use image::{DynamicImage, ImageBuffer, ImageFormat};
use rand::RngCore;

/// MIME types we'll request from `wl-paste`, in preference order.
/// PNG first (lossless), then JPEG (compact), then WebP / GIF.
const PREFERRED_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Read an image from the system clipboard and write it to a fresh
/// file under [`std::env::temp_dir`]. Returns the absolute path of
/// the written file on success.
///
/// File naming: `aj-clipboard-<random-hex>.<ext>`, where `<ext>`
/// matches the content (`png` / `jpg` / `webp` / `gif`). The file
/// is NOT auto-deleted — its lifetime must outlive the editor
/// session so a subsequent `read_file` tool call can open it.
pub fn read_image_to_tempfile() -> Option<PathBuf> {
    // Try each backend in priority order; first one with bytes wins.
    // arboard handles macOS, Windows, and X11 Linux. On Wayland its
    // image support is unreliable, so we fall back to `wl-paste`.
    // Inside WSL the X11 path can't reach the Windows clipboard, so
    // we additionally fall back to a PowerShell screenshot.
    let (mime, bytes) = read_via_arboard()
        .or_else(|| {
            if is_wayland() {
                read_via_wl_paste()
            } else {
                None
            }
        })
        .or_else(|| {
            if is_wsl() {
                read_via_powershell()
            } else {
                None
            }
        })?;

    let ext = extension_for_mime(mime)?;
    let dest = temp_dir().join(generate_filename(ext));
    if let Err(e) = fs::write(&dest, &bytes) {
        tracing::debug!(
            "clipboard: failed to write tempfile {}: {e}",
            dest.display()
        );
        return None;
    }
    Some(dest)
}

/// Map a supported image MIME type to the file extension used in
/// the tempfile name. `None` for any MIME not in the supported set.
fn extension_for_mime(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// Generate a fresh filename of the form `aj-clipboard-<hex>.<ext>`.
/// The hex segment is 16 chars of cryptographic randomness — enough
/// to make accidental collisions in a single `temp_dir` effectively
/// impossible.
fn generate_filename(ext: &str) -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("aj-clipboard-{hex}.{ext}")
}

fn temp_dir() -> PathBuf {
    env::temp_dir()
}

/// arboard returns raw RGBA pixels; we always re-encode as PNG
/// (lossless) before handing bytes upstream.
fn read_via_arboard() -> Option<(&'static str, Vec<u8>)> {
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
    Some(("image/png", out.into_inner()))
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

/// Try to pull an image from a Wayland clipboard via `wl-paste`.
/// Returns `(mime_type, bytes)` on success, where `mime_type` is
/// one of [`PREFERRED_MIME_TYPES`]. When the clipboard only offers
/// an image in an unsupported format (e.g. `image/bmp` from WSLg),
/// the bytes are re-encoded as PNG via the `image` crate.
fn read_via_wl_paste() -> Option<(&'static str, Vec<u8>)> {
    let listed = list_wl_paste_types()?;
    let (chosen, supported) = negotiate_clipboard_type(&listed)?;

    let bytes = fetch_wl_paste(&chosen)?;
    if bytes.is_empty() {
        tracing::debug!("clipboard: wl-paste returned empty data for type {chosen}");
        return None;
    }

    if let Some(supported_mime) = supported {
        return Some((supported_mime, bytes));
    }

    // Unsupported source MIME — re-encode through the `image` crate
    // so downstream MIME sniffing in read_file accepts the result.
    // BMP and TIFF features are enabled in the workspace `image`
    // dep specifically so this branch can decode common Windows-
    // clipboard formats (WSLg surfaces screenshots as BMP).
    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!("clipboard: re-encode decode failed for {chosen}: {e}");
            return None;
        }
    };
    let mut out = std::io::Cursor::new(Vec::<u8>::new());
    if let Err(e) = img.write_to(&mut out, ImageFormat::Png) {
        tracing::debug!("clipboard: re-encode to PNG failed for {chosen}: {e}");
        return None;
    }
    Some(("image/png", out.into_inner()))
}

/// Run `wl-paste --list-types` and return the offered types as
/// trimmed, lowercased strings. `None` if `wl-paste` is missing,
/// failed, or produced an empty list.
fn list_wl_paste_types() -> Option<Vec<String>> {
    let out = match Command::new("wl-paste").arg("--list-types").output() {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("clipboard: wl-paste not installed");
            return None;
        }
        Err(e) => {
            tracing::debug!("clipboard: wl-paste --list-types failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            "clipboard: wl-paste --list-types exited {}",
            out.status.code().unwrap_or(-1)
        );
        return None;
    }
    let listed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if listed.is_empty() {
        tracing::debug!("clipboard: wl-paste --list-types produced no types");
        return None;
    }
    Some(listed)
}

/// Fetch a specific MIME type's payload via
/// `wl-paste --type <mime> --no-newline`. `--no-newline` keeps
/// `wl-paste` from appending a stray byte to binary data.
fn fetch_wl_paste(mime: &str) -> Option<Vec<u8>> {
    let out = match Command::new("wl-paste")
        .args(["--type", mime, "--no-newline"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("clipboard: wl-paste --type {mime} failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            "clipboard: wl-paste --type {mime} exited {}",
            out.status.code().unwrap_or(-1)
        );
        return None;
    }
    Some(out.stdout)
}

/// Strip a parameter suffix (e.g. `; charset=utf-8`) from a MIME
/// type and lowercase the base.
fn normalize_mime(raw: &str) -> String {
    raw.split(';').next().unwrap_or("").trim().to_lowercase()
}

/// Pick a directly-supported MIME type from `offered`, preferring
/// the order in [`PREFERRED_MIME_TYPES`]. Returns the matching
/// `&'static str` from that constant, or `None` when no offered
/// type matches.
fn pick_preferred_mime(offered: &[String]) -> Option<&'static str> {
    let normalized: Vec<String> = offered.iter().map(|s| normalize_mime(s)).collect();
    for &candidate in PREFERRED_MIME_TYPES {
        if normalized.iter().any(|n| n == candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Decide what to fetch from the clipboard given the offered MIME
/// list. Returns `(type_to_request, supported_match)` where
/// `supported_match` is `Some(mime)` when the chosen type is
/// directly supported and `None` when the caller must re-encode
/// the bytes as PNG. `None` overall means no `image/*` was offered.
fn negotiate_clipboard_type(offered: &[String]) -> Option<(String, Option<&'static str>)> {
    if let Some(mime) = pick_preferred_mime(offered) {
        return Some((mime.to_string(), Some(mime)));
    }
    // Fallback: any `image/*` offering. We'll request it verbatim
    // and re-encode the bytes as PNG.
    let any_image = offered
        .iter()
        .map(|s| normalize_mime(s))
        .find(|s| s.starts_with("image/"))?;
    tracing::debug!(
        "clipboard: no preferred MIME offered, falling back to {any_image} with re-encode"
    );
    Some((any_image, None))
}

/// Pull an image from the Windows clipboard via PowerShell.
/// PowerShell writes a PNG to a throwaway temp path; we read those
/// bytes back, clean up, and return them so the top-level
/// orchestrator can write the final tempfile.
fn read_via_powershell() -> Option<(&'static str, Vec<u8>)> {
    let staging = temp_dir().join(generate_filename("png"));

    let win_path = match Command::new("wslpath").arg("-w").arg(&staging).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            tracing::debug!(
                "clipboard: wslpath exited {}",
                o.status.code().unwrap_or(-1)
            );
            return None;
        }
        Err(e) => {
            tracing::debug!("clipboard: wslpath failed: {e}");
            return None;
        }
    };
    if win_path.is_empty() {
        return None;
    }

    // Single-quote the path for PowerShell; escape embedded quotes
    // by doubling per PowerShell's single-quoted-string rules.
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
            let _ = fs::remove_file(&staging);
            return None;
        }
    };
    if !out.status.success() {
        let _ = fs::remove_file(&staging);
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains("ok") || !staging.exists() {
        let _ = fs::remove_file(&staging);
        return None;
    }
    let bytes = match fs::read(&staging) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("clipboard: failed to read PowerShell staging file: {e}");
            let _ = fs::remove_file(&staging);
            return None;
        }
    };
    let _ = fs::remove_file(&staging);
    if bytes.is_empty() {
        return None;
    }
    Some(("image/png", bytes))
}

#[cfg(test)]
mod tests {
    //! The subprocess-driven backends (`wl-paste`, PowerShell) and
    //! arboard's OS clipboard access can't be unit-tested without
    //! mocking the platform. Those paths are exercised by hand.
    //! These tests cover the pure helpers: filename generation,
    //! MIME-to-extension mapping, and clipboard-type negotiation.

    use aj_tools::image::SUPPORTED_MIME_TYPES;

    use super::*;

    #[test]
    fn extension_for_mime_round_trips() {
        for &mime in SUPPORTED_MIME_TYPES {
            let ext = extension_for_mime(mime)
                .unwrap_or_else(|| panic!("no extension for supported MIME {mime}"));
            assert!(!ext.is_empty(), "empty extension for {mime}");
        }
        assert_eq!(extension_for_mime("image/bmp"), None);
        assert_eq!(extension_for_mime("text/plain"), None);
    }

    #[test]
    fn generate_filename_matches_pattern() {
        for ext in ["png", "jpg", "webp", "gif"] {
            let name = generate_filename(ext);
            assert!(name.starts_with("aj-clipboard-"), "{name}");
            let suffix = format!(".{ext}");
            assert!(name.ends_with(&suffix), "{name}");
            let hex = &name["aj-clipboard-".len()..name.len() - suffix.len()];
            assert_eq!(hex.len(), 16, "{name}");
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        }
    }

    #[test]
    fn generate_filename_is_unique() {
        let a = generate_filename("png");
        let b = generate_filename("png");
        assert_ne!(a, b);
    }

    #[test]
    fn preferred_mime_types_match_supported_set() {
        // The preference list must be a subset of the globally
        // supported set — otherwise we'd request a type that
        // downstream MIME sniffing would later reject.
        for &m in PREFERRED_MIME_TYPES {
            assert!(
                SUPPORTED_MIME_TYPES.contains(&m),
                "{m} missing from SUPPORTED_MIME_TYPES"
            );
        }
        // And conversely, every supported type should be requestable.
        for &m in SUPPORTED_MIME_TYPES {
            assert!(
                PREFERRED_MIME_TYPES.contains(&m),
                "{m} missing from PREFERRED_MIME_TYPES"
            );
        }
    }

    #[test]
    fn pick_preferred_mime_prefers_png_over_jpeg() {
        let offered = vec!["image/jpeg".to_string(), "image/png".to_string()];
        assert_eq!(pick_preferred_mime(&offered), Some("image/png"));
    }

    #[test]
    fn pick_preferred_mime_normalizes_charset_suffix() {
        let offered = vec!["image/png; charset=utf-8".to_string()];
        assert_eq!(pick_preferred_mime(&offered), Some("image/png"));
    }

    #[test]
    fn pick_preferred_mime_returns_none_for_text_only() {
        let offered = vec!["text/plain".to_string(), "UTF8_STRING".to_lowercase()];
        assert_eq!(pick_preferred_mime(&offered), None);
    }

    #[test]
    fn negotiate_returns_supported_directly() {
        let offered = vec!["image/jpeg".to_string()];
        let (req, supported) = negotiate_clipboard_type(&offered).unwrap();
        assert_eq!(req, "image/jpeg");
        assert_eq!(supported, Some("image/jpeg"));
    }

    #[test]
    fn negotiate_falls_back_to_any_image_for_reencode() {
        let offered = vec!["image/bmp".to_string(), "text/plain".to_string()];
        let (req, supported) = negotiate_clipboard_type(&offered).unwrap();
        assert_eq!(req, "image/bmp");
        assert_eq!(supported, None);
    }

    #[test]
    fn negotiate_returns_none_without_any_image() {
        let offered = vec!["text/plain".to_string(), "text/html".to_string()];
        assert!(negotiate_clipboard_type(&offered).is_none());
    }

    /// The re-encode fallback in `read_via_wl_paste` decodes whatever
    /// `wl-paste` returns through `image::load_from_memory`, then
    /// encodes as PNG. That only works if the workspace `image`
    /// dep includes `bmp` (and `tiff`) features — WSLg surfaces
    /// Windows screenshots as BMP, which is the marquee case for
    /// this branch. This test fails loudly if those features get
    /// stripped from `Cargo.toml` later.
    #[test]
    fn image_crate_decodes_bmp_for_reencode_fallback() {
        // Hand-rolled 1×1 24-bit BMP. Header (14) + DIB header (40)
        // + pixel data (1 pixel, 4-byte aligned).
        let bmp: &[u8] = &[
            // BITMAPFILEHEADER
            b'B', b'M', // signature
            0x3a, 0x00, 0x00, 0x00, // file size (58)
            0x00, 0x00, 0x00, 0x00, // reserved
            0x36, 0x00, 0x00, 0x00, // pixel data offset (54)
            // BITMAPINFOHEADER (40 bytes)
            0x28, 0x00, 0x00, 0x00, // header size
            0x01, 0x00, 0x00, 0x00, // width = 1
            0x01, 0x00, 0x00, 0x00, // height = 1
            0x01, 0x00, // planes = 1
            0x18, 0x00, // bpp = 24
            0x00, 0x00, 0x00, 0x00, // compression = BI_RGB
            0x04, 0x00, 0x00, 0x00, // image size (4 bytes of pixel data)
            0x00, 0x00, 0x00, 0x00, // x ppm
            0x00, 0x00, 0x00, 0x00, // y ppm
            0x00, 0x00, 0x00, 0x00, // colors used
            0x00, 0x00, 0x00, 0x00, // important colors
            // pixel data: one BGR pixel + 1 byte row padding
            0xff, 0x00, 0x00, 0x00,
        ];
        let img = image::load_from_memory(bmp).expect("decode BMP");
        let mut out = std::io::Cursor::new(Vec::<u8>::new());
        img.write_to(&mut out, ImageFormat::Png)
            .expect("encode PNG");
        assert!(!out.into_inner().is_empty());
    }
}
