//! Image MIME sniffing and resizing for tool image attachments.
//!
//! Tools that surface image data on a [`ToolOutcome`](aj_agent::tool::ToolOutcome)
//! use this module to:
//!
//! 1. Confirm the input is a supported image format via
//!    [`detect_mime_type`] / [`detect_mime_type_from_file`].
//! 2. Bring the payload below the provider's per-image budget via
//!    [`resize_image`], which combines pixel and base64-byte limits
//!    and may re-encode as JPEG to fit.
//! 3. Annotate prompts with [`format_dimension_note`] when the
//!    displayed image is smaller than the source, so models doing
//!    pixel-coordinate reasoning can scale back up.
//!
//! The sniffer is content-based (magic-byte): callers should never
//! trust caller-provided extensions or MIME strings without
//! re-running detection.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ImageEncoder, ImageReader, RgbaImage, imageops};

// ---------------------------------------------------------------------------
// MIME sniffing
// ---------------------------------------------------------------------------

/// Supported image MIME types. Anything outside this set is treated
/// as a non-image by callers (read_file, clipboard paste).
pub const SUPPORTED_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Number of leading bytes inspected by [`detect_mime_type_from_file`].
/// Sized to span the PNG signature plus enough chunk headers to spot
/// an `acTL` chunk before the first `IDAT` in an animated PNG.
const SNIFF_BYTES: usize = 4100;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// MIME sniff result. `None` means "not a supported image".
///
/// Rules:
/// - JPEG: bytes `0xFF 0xD8 0xFF`, with byte 3 ≠ `0xF7` (excludes
///   JPEG 2000 / unusual variants).
/// - PNG: 8-byte signature followed by a valid IHDR. Animated PNGs
///   (an `acTL` chunk appearing before `IDAT`) are rejected.
/// - GIF: ASCII `GIF` at offset 0.
/// - WebP: ASCII `RIFF` at offset 0 and ASCII `WEBP` at offset 8.
///
/// At most the first [`SNIFF_BYTES`] of input are inspected.
pub fn detect_mime_type(bytes: &[u8]) -> Option<&'static str> {
    let head = &bytes[..bytes.len().min(SNIFF_BYTES)];

    if starts_with(head, &[0xff, 0xd8, 0xff]) {
        return if head.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(head, &PNG_SIGNATURE) {
        return if is_png(head) && !is_animated_png(head) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(head, 0, b"GIF") {
        return Some("image/gif");
    }
    if starts_with_ascii(head, 0, b"RIFF") && starts_with_ascii(head, 8, b"WEBP") {
        return Some("image/webp");
    }
    None
}

/// Same as [`detect_mime_type`] but reads the first ~4KB of a file
/// from disk. Returns `None` for any error (missing file, unreadable,
/// non-image header).
pub fn detect_mime_type_from_file(path: &Path) -> Option<&'static str> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; SNIFF_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    detect_mime_type(&buf)
}

fn starts_with(buffer: &[u8], prefix: &[u8]) -> bool {
    buffer.len() >= prefix.len() && &buffer[..prefix.len()] == prefix
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &[u8]) -> bool {
    buffer
        .get(offset..offset + text.len())
        .is_some_and(|slice| slice == text)
}

fn read_u32_be(buffer: &[u8], offset: usize) -> Option<u32> {
    let slice = buffer.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn is_png(buffer: &[u8]) -> bool {
    // After the signature, the first chunk must be a 13-byte IHDR.
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == Some(13)
        && starts_with_ascii(buffer, 12, b"IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    // Walk chunks looking for `acTL` (animation control) before any
    // `IDAT` (image data). Either chunk type's absence within the
    // sniff window is treated as "not animated" — pathological inputs
    // larger than the window are out of scope.
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let Some(chunk_length) = read_u32_be(buffer, offset) else {
            return false;
        };
        let type_offset = offset + 4;
        if starts_with_ascii(buffer, type_offset, b"acTL") {
            return true;
        }
        if starts_with_ascii(buffer, type_offset, b"IDAT") {
            return false;
        }
        // length + 4-byte length field + 4-byte type field + 4-byte CRC
        let Some(advance) = usize::try_from(chunk_length)
            .ok()
            .and_then(|n| n.checked_add(12))
        else {
            return false;
        };
        let Some(next_offset) = offset.checked_add(advance) else {
            return false;
        };
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

/// 4.5 MiB. Provides headroom below the common 5 MB provider limit
/// on per-image base64 payloads.
const DEFAULT_MAX_BYTES: usize = (4 * 1024 * 1024) + (512 * 1024);

/// Tunable thresholds for [`resize_image`].
pub struct ResizeOptions {
    /// Maximum width in pixels. Default: 2000.
    pub max_width: u32,
    /// Maximum height in pixels. Default: 2000.
    pub max_height: u32,
    /// Maximum size of the base64-encoded payload in bytes.
    /// Default: 4.5 * 1024 * 1024 (provides headroom below provider
    /// 5MB limits).
    pub max_bytes: usize,
    /// Initial JPEG quality (1-100). Default: 80.
    pub jpeg_quality: u8,
}

impl Default for ResizeOptions {
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: DEFAULT_MAX_BYTES,
            jpeg_quality: 80,
        }
    }
}

/// Output of [`resize_image`]: a payload ready to attach to a tool
/// result, plus enough metadata to populate
/// [`ToolDetails::Image`](aj_agent::tool::ToolDetails).
pub struct ResizedImage {
    /// Base64-encoded payload bytes ready to drop into
    /// [`aj_models::types::ImageContent::data`].
    pub data: String,
    /// MIME type of the encoded `data` (may differ from the source if
    /// JPEG re-encoding was needed to fit).
    pub mime_type: String,
    /// Dimensions before any resize.
    pub original_width: u32,
    pub original_height: u32,
    /// Dimensions after resize. Equal to original when `was_resized`
    /// is `false`.
    pub width: u32,
    pub height: u32,
    /// Whether the image was re-encoded (resize, format swap, or
    /// quality reduction). `false` means `data` is the source bytes
    /// base64-encoded.
    pub was_resized: bool,
}

/// Resize an image to fit within the configured pixel and byte
/// budgets. Returns `None` if no combination of size + format +
/// quality can fit under `max_bytes` (in which case callers should
/// emit a textual "image omitted" note in place of the attachment).
///
/// Strategy:
/// 1. If the source is already under all limits, return it base64
///    encoded as-is with `was_resized: false`.
/// 2. Otherwise rescale to fit `max_width` × `max_height` preserving
///    aspect ratio (Lanczos3 resampling).
/// 3. At the current dimensions, encode as PNG and as JPEG at a
///    descending quality ladder; return the first encoding that fits
///    under `max_bytes`.
/// 4. If none fit, shrink dimensions by a 0.75 factor and try again;
///    bail out with `None` once `(1, 1)` is reached without success.
pub fn resize_image(
    bytes: &[u8],
    mime_type: &str,
    options: &ResizeOptions,
) -> Option<ResizedImage> {
    // NOTE: EXIF orientation correction not yet implemented — phone
    // photos may appear rotated.

    let decoded = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .to_rgba8();
    let original_width = decoded.width();
    let original_height = decoded.height();
    let source_b64_size = encoded_base64_len(bytes.len());

    if original_width <= options.max_width
        && original_height <= options.max_height
        && source_b64_size < options.max_bytes
    {
        return Some(ResizedImage {
            data: BASE64.encode(bytes),
            mime_type: mime_type.to_string(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut current_width, mut current_height) = scale_to_fit(
        original_width,
        original_height,
        options.max_width,
        options.max_height,
    );

    let quality_steps = dedup_qualities(&[options.jpeg_quality, 85, 70, 55, 40]);

    loop {
        if let Some(candidate) = first_fit(
            &decoded,
            current_width,
            current_height,
            &quality_steps,
            options.max_bytes,
        ) {
            return Some(ResizedImage {
                data: candidate.data,
                mime_type: candidate.mime_type,
                original_width,
                original_height,
                width: current_width,
                height: current_height,
                was_resized: true,
            });
        }

        if current_width == 1 && current_height == 1 {
            return None;
        }
        let next_width = if current_width == 1 {
            1
        } else {
            (u64::from(current_width) * 3 / 4)
                .max(1)
                .try_into()
                .unwrap_or(1)
        };
        let next_height = if current_height == 1 {
            1
        } else {
            (u64::from(current_height) * 3 / 4)
                .max(1)
                .try_into()
                .unwrap_or(1)
        };
        if next_width == current_width && next_height == current_height {
            return None;
        }
        current_width = next_width;
        current_height = next_height;
    }
}

/// Build the dimension annotation included alongside resized images.
/// Returns `None` when no resize occurred; otherwise emits a fixed
/// wording so prompt-engineered models keep behaving the same:
///
/// `[Image: original WxH, displayed at WxH. Multiply coordinates by S.SS to map to original image.]`
pub fn format_dimension_note(resized: &ResizedImage) -> Option<String> {
    if !resized.was_resized {
        return None;
    }
    let scale = f64::from(resized.original_width) / f64::from(resized.width.max(1));
    Some(format!(
        "[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]",
        ow = resized.original_width,
        oh = resized.original_height,
        w = resized.width,
        h = resized.height,
    ))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct EncodedCandidate {
    data: String,
    encoded_size: usize,
    mime_type: String,
}

/// Length of the base64 encoding for `raw_len` bytes (standard
/// padded alphabet). Used to decide whether a payload already fits
/// without paying for an actual base64 encode.
fn encoded_base64_len(raw_len: usize) -> usize {
    raw_len.div_ceil(3) * 4
}

fn dedup_qualities(steps: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(steps.len());
    for &q in steps {
        if !out.contains(&q) {
            out.push(q);
        }
    }
    out
}

fn scale_to_fit(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let mut w = width;
    let mut h = height;
    if w > max_w {
        let scaled = u64::from(h) * u64::from(max_w) / u64::from(w.max(1));
        h = scaled.try_into().unwrap_or(1).max(1);
        w = max_w;
    }
    if h > max_h {
        let scaled = u64::from(w) * u64::from(max_h) / u64::from(h.max(1));
        w = scaled.try_into().unwrap_or(1).max(1);
        h = max_h;
    }
    (w.max(1), h.max(1))
}

/// Encode `source` at `width`×`height` and return the first candidate
/// that fits under `max_bytes`. Candidates are tried in order: PNG
/// first (lossless when it fits), then JPEG at each step in
/// `qualities`. Returns `None` if no candidate fits.
///
/// First-fit (not best-fit) is intentional: when the PNG encoding
/// fits the budget we prefer it over a smaller-but-lossy JPEG, so
/// images of text/diagrams retain readability whenever there is
/// headroom for them.
fn first_fit(
    source: &RgbaImage,
    width: u32,
    height: u32,
    qualities: &[u8],
    max_bytes: usize,
) -> Option<EncodedCandidate> {
    let resized = imageops::resize(source, width, height, imageops::FilterType::Lanczos3);

    if let Some(png) = encode_png(&resized)
        && png.encoded_size < max_bytes
    {
        return Some(png);
    }
    for &quality in qualities {
        if let Some(jpeg) = encode_jpeg(&resized, quality)
            && jpeg.encoded_size < max_bytes
        {
            return Some(jpeg);
        }
    }
    None
}

fn encode_png(image: &RgbaImage) -> Option<EncodedCandidate> {
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
        .write_image(
            image,
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(finalize_candidate(&buf, "image/png"))
}

fn encode_jpeg(image: &RgbaImage, quality: u8) -> Option<EncodedCandidate> {
    // JPEG cannot represent an alpha channel; flatten to RGB before
    // encoding so the codec doesn't reject the input.
    let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .write_image(
            &rgb,
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(finalize_candidate(&buf, "image/jpeg"))
}

fn finalize_candidate(raw: &[u8], mime_type: &str) -> EncodedCandidate {
    let data = BASE64.encode(raw);
    let encoded_size = data.len();
    EncodedCandidate {
        data,
        encoded_size,
        mime_type: mime_type.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use image::{ImageFormat, Rgba};
    use std::io::Write;

    fn jpeg_header() -> Vec<u8> {
        vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]
    }

    /// Minimal valid PNG: signature + IHDR chunk + IDAT marker. We
    /// don't need a real image here — just enough bytes for the
    /// sniffer to commit to PNG and stop walking chunks.
    fn png_header() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PNG_SIGNATURE);
        // IHDR: length 13, type "IHDR", 13 bytes of dummy header
        // data, 4 bytes CRC (unused by the sniffer).
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&[0u8; 13]);
        out.extend_from_slice(&[0u8; 4]);
        // IDAT chunk header — length 0, type "IDAT".
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"IDAT");
        out
    }

    /// PNG with an `acTL` chunk inserted before `IDAT`. Sniffer must
    /// reject this even though the signature is valid.
    fn animated_png_header() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PNG_SIGNATURE);
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&[0u8; 13]);
        out.extend_from_slice(&[0u8; 4]);
        // acTL: length 8, type "acTL", 8 bytes data, 4 bytes CRC.
        out.extend_from_slice(&8u32.to_be_bytes());
        out.extend_from_slice(b"acTL");
        out.extend_from_slice(&[0u8; 8]);
        out.extend_from_slice(&[0u8; 4]);
        // IDAT chunk header trailing the acTL.
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"IDAT");
        out
    }

    fn gif_header() -> Vec<u8> {
        b"GIF89a".to_vec()
    }

    fn webp_header() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(b"WEBP");
        out
    }

    #[test]
    fn detect_mime_type_recognises_jpeg() {
        assert_eq!(detect_mime_type(&jpeg_header()), Some("image/jpeg"));
    }

    #[test]
    fn detect_mime_type_rejects_jpeg_with_byte3_f7() {
        let mut bytes = jpeg_header();
        bytes[3] = 0xf7;
        assert_eq!(detect_mime_type(&bytes), None);
    }

    #[test]
    fn detect_mime_type_recognises_png() {
        assert_eq!(detect_mime_type(&png_header()), Some("image/png"));
    }

    #[test]
    fn detect_mime_type_rejects_animated_png() {
        assert_eq!(detect_mime_type(&animated_png_header()), None);
    }

    #[test]
    fn detect_mime_type_recognises_gif() {
        assert_eq!(detect_mime_type(&gif_header()), Some("image/gif"));
    }

    #[test]
    fn detect_mime_type_recognises_webp() {
        assert_eq!(detect_mime_type(&webp_header()), Some("image/webp"));
    }

    #[test]
    fn detect_mime_type_rejects_non_image() {
        assert_eq!(detect_mime_type(b"hello world, this is not an image"), None);
        assert_eq!(detect_mime_type(&[0u8; 32]), None);
    }

    #[test]
    fn detect_mime_type_from_file_round_trips() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let mut handle = tmp.reopen().expect("reopen");
        handle.write_all(&png_header()).expect("write");
        handle.flush().expect("flush");
        drop(handle);

        assert_eq!(detect_mime_type_from_file(tmp.path()), Some("image/png"));
    }

    #[test]
    fn detect_mime_type_from_file_returns_none_for_missing() {
        let path = std::path::PathBuf::from("/this/path/should/not/exist/aj-test");
        assert_eq!(detect_mime_type_from_file(&path), None);
    }

    /// Build a real `width`×`height` PNG via the `image` crate and
    /// return its encoded bytes.
    fn make_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(width, height);
        // A faintly varying pattern so PNG can't compress to zero.
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = u8::try_from((x ^ y) & 0xff).unwrap_or(0);
            *px = Rgba([r, 128, 64, 255]);
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .expect("encode png");
        buf
    }

    /// Build a `width`×`height` PNG filled with a single solid color.
    /// PNG's run-length filter + DEFLATE compress this to a tiny
    /// payload regardless of the source dimensions, which lets tests
    /// reason about "PNG fits the budget" without doing the math.
    fn make_solid_png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(width, height, Rgba([180, 200, 220, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .expect("encode png");
        buf
    }

    #[test]
    fn resize_image_passthrough_when_within_limits() {
        let bytes = make_png(100, 100);
        let resized =
            resize_image(&bytes, "image/png", &ResizeOptions::default()).expect("resize result");
        assert!(!resized.was_resized);
        assert_eq!(resized.original_width, 100);
        assert_eq!(resized.original_height, 100);
        assert_eq!(resized.width, 100);
        assert_eq!(resized.height, 100);
        assert_eq!(resized.mime_type, "image/png");
        assert_eq!(resized.data, BASE64.encode(&bytes));
    }

    #[test]
    fn resize_image_shrinks_large_input_preserving_aspect_ratio() {
        let bytes = make_png(4000, 3000);
        let resized =
            resize_image(&bytes, "image/png", &ResizeOptions::default()).expect("resize result");
        assert!(resized.was_resized);
        assert_eq!(resized.original_width, 4000);
        assert_eq!(resized.original_height, 3000);
        // 2000 / 4000 = 0.5; height should scale to ~1500.
        assert!(resized.width <= 2000, "width: {}", resized.width);
        assert!(resized.height <= 2000, "height: {}", resized.height);
        assert_eq!(resized.width, 2000);
        assert_eq!(resized.height, 1500);
    }

    /// First-fit semantics: when a PNG encoding of the resized image
    /// fits the byte budget, we keep PNG even though a low-quality
    /// JPEG would be smaller. This protects readability of text /
    /// diagrams whenever there is budget headroom.
    #[test]
    fn resize_image_prefers_png_over_jpeg_when_png_fits() {
        // Solid-color PNG: compresses to a tiny payload, so PNG fits
        // comfortably under the default byte budget and first-fit
        // returns it before the JPEG ladder is tried.
        let bytes = make_solid_png(4000, 3000);
        let resized =
            resize_image(&bytes, "image/png", &ResizeOptions::default()).expect("resize result");
        assert!(resized.was_resized);
        assert_eq!(resized.mime_type, "image/png");
    }

    /// When the byte budget is so tight that no PNG fits, the
    /// algorithm should fall back to JPEG.
    #[test]
    fn resize_image_falls_back_to_jpeg_when_png_does_not_fit() {
        let bytes = make_png(800, 600);
        let opts = ResizeOptions {
            // Tiny budget that the synthetic noise-pattern PNG will
            // exceed at any reasonable dimension, forcing JPEG.
            max_bytes: 8 * 1024,
            ..ResizeOptions::default()
        };
        let resized = resize_image(&bytes, "image/png", &opts).expect("resize result");
        assert!(resized.was_resized);
        assert_eq!(resized.mime_type, "image/jpeg");
    }

    #[test]
    fn format_dimension_note_returns_none_when_not_resized() {
        let resized = ResizedImage {
            data: String::new(),
            mime_type: "image/png".into(),
            original_width: 100,
            original_height: 100,
            width: 100,
            height: 100,
            was_resized: false,
        };
        assert!(format_dimension_note(&resized).is_none());
    }

    #[test]
    fn format_dimension_note_formats_when_resized() {
        let resized = ResizedImage {
            data: String::new(),
            mime_type: "image/png".into(),
            original_width: 1920,
            original_height: 1080,
            width: 800,
            height: 450,
            was_resized: true,
        };
        let note = format_dimension_note(&resized).expect("note");
        assert_eq!(
            note,
            "[Image: original 1920x1080, displayed at 800x450. Multiply coordinates by 2.40 to map to original image.]"
        );
    }
}
