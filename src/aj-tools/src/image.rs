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
    let decoded = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .to_rgba8();
    // Apply EXIF orientation before measuring dimensions so the
    // budget short-circuit and the dimension-note coordinate scale
    // both work in display-oriented coordinates.
    let orientation = exif::read_exif_orientation(bytes, mime_type);
    let decoded = exif::apply_exif_orientation(decoded, orientation);
    let original_width = decoded.width();
    let original_height = decoded.height();
    let source_b64_size = encoded_base64_len(bytes.len());

    if original_width <= options.max_width
        && original_height <= options.max_height
        && source_b64_size < options.max_bytes
        && orientation == 1
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

/// Wrap a source image's bytes into a [`ResizedImage`] without any
/// resize or re-encoding: dimensions are decoded from the source,
/// `data` is the source bytes base64-encoded, and `was_resized` is
/// `false`. Returns `None` if the bytes cannot be decoded.
///
/// Used by `read_file` when the user disables the inline image
/// budget via `image_auto_resize = false` in `~/.aj/config.toml`.
/// Callers accept that the resulting attachment may exceed the
/// provider's per-image size limit and be rejected on the wire.
///
/// EXIF orientation is intentionally preserved: because the source
/// bytes are forwarded verbatim, the Orientation tag stays attached
/// and vision models honor it themselves. Re-encoding to "bake in"
/// the rotation would contradict the explicit `image_auto_resize =
/// false` opt-in. The returned `original_width`/`original_height`
/// reflect the sensor-orientation dimensions reported by the
/// decoder, which matches the on-wire pixel buffer.
pub fn passthrough_image(bytes: &[u8], mime_type: &str) -> Option<ResizedImage> {
    let (width, height) = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(ResizedImage {
        data: BASE64.encode(bytes),
        mime_type: mime_type.to_string(),
        original_width: width,
        original_height: height,
        width,
        height,
        was_resized: false,
    })
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
// EXIF orientation
// ---------------------------------------------------------------------------

mod exif {
    use image::{RgbaImage, imageops};

    /// EXIF Orientation tag value.
    const TAG_ORIENTATION: u16 = 0x0112;
    /// EXIF SHORT type — what Orientation is always encoded as.
    const TYPE_SHORT: u16 = 3;

    /// Read EXIF orientation tag (0x0112) from a JPEG APP1 segment or
    /// a WebP EXIF chunk. Returns 1 (no rotation) when the tag isn't
    /// present, the file format doesn't carry EXIF, or the metadata
    /// is malformed. Valid return values are 1..=8 per the EXIF spec.
    pub(super) fn read_exif_orientation(bytes: &[u8], mime_type: &str) -> u8 {
        let raw = match mime_type {
            "image/jpeg" => parse_jpeg(bytes),
            "image/webp" => parse_webp(bytes),
            _ => None,
        };
        match raw {
            Some(o) if (1..=8).contains(&o) => o,
            _ => 1,
        }
    }

    /// Apply the geometric transform implied by EXIF orientation
    /// `1..=8` to `image`. Orientation 1 returns the image unchanged.
    /// Unknown values are treated as 1.
    pub(super) fn apply_exif_orientation(image: RgbaImage, orientation: u8) -> RgbaImage {
        match orientation {
            2 => imageops::flip_horizontal(&image),
            3 => imageops::rotate180(&image),
            4 => imageops::flip_vertical(&image),
            5 => imageops::flip_horizontal(&imageops::rotate90(&image)),
            6 => imageops::rotate90(&image),
            7 => imageops::flip_horizontal(&imageops::rotate270(&image)),
            8 => imageops::rotate270(&image),
            _ => image,
        }
    }

    /// Walk JPEG segments looking for an APP1 segment whose payload
    /// starts with `"Exif\0\0"`, then parse orientation from the TIFF
    /// header that follows.
    fn parse_jpeg(bytes: &[u8]) -> Option<u8> {
        if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
            return None;
        }
        let mut offset = 2;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xFF {
                return None;
            }
            // Skip stuffing / fill bytes (`0xFF 0xFF...`).
            if bytes[offset + 1] == 0xFF {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            // SOI/EOI/RSTn carry no length field; stop on EOI/SOS.
            // Note: 0xD8 (SOI) is only valid at file start; 0xD9 is EOI;
            // 0xDA (SOS) marks the start of compressed scan data and
            // any subsequent EXIF would be unreachable.
            if marker == 0xD9 || marker == 0xDA {
                return None;
            }
            let length = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
            if length < 2 {
                return None;
            }
            let segment_start = offset + 2;
            let payload_start = segment_start + 2;
            let segment_end = segment_start + length;
            if segment_end > bytes.len() {
                return None;
            }
            if marker == 0xE1 {
                let payload = &bytes[payload_start..segment_end];
                if payload.len() >= 6 && &payload[..6] == b"Exif\0\0" {
                    return parse_tiff(&payload[6..]);
                }
            }
            offset = segment_end;
        }
        None
    }

    /// Walk WebP RIFF chunks looking for the `EXIF` chunk, then parse
    /// orientation from the TIFF header it contains. Some encoders
    /// prefix the TIFF with the JPEG-style `"Exif\0\0"` header — skip
    /// it when present.
    fn parse_webp(bytes: &[u8]) -> Option<u8> {
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
            return None;
        }
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let length = usize::try_from(read_u32_le(bytes, offset + 4)?).ok()?;
            let payload_start = offset + 8;
            let payload_end = payload_start.checked_add(length)?;
            if payload_end > bytes.len() {
                return None;
            }
            if id == b"EXIF" {
                let mut payload = &bytes[payload_start..payload_end];
                if payload.len() >= 6 && &payload[..6] == b"Exif\0\0" {
                    payload = &payload[6..];
                }
                return parse_tiff(payload);
            }
            // RIFF chunks are padded to an even length.
            let advance = length + (length & 1);
            offset = payload_start.checked_add(advance)?;
        }
        None
    }

    /// Parse the Orientation tag out of a TIFF header (`tiff` starts
    /// at the byte-order mark). Returns `None` if the header is
    /// malformed or the tag is absent.
    fn parse_tiff(tiff: &[u8]) -> Option<u8> {
        if tiff.len() < 8 {
            return None;
        }
        let little_endian = match &tiff[..2] {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let magic = read_u16(tiff, 2, little_endian)?;
        if magic != 0x002A {
            return None;
        }
        let ifd_offset = usize::try_from(read_u32(tiff, 4, little_endian)?).ok()?;
        if ifd_offset + 2 > tiff.len() {
            return None;
        }
        let entry_count = usize::from(read_u16(tiff, ifd_offset, little_endian)?);
        let entries_start = ifd_offset + 2;
        if entries_start + entry_count * 12 > tiff.len() {
            return None;
        }
        for i in 0..entry_count {
            let e = entries_start + i * 12;
            let tag = read_u16(tiff, e, little_endian)?;
            if tag != TAG_ORIENTATION {
                continue;
            }
            let field_type = read_u16(tiff, e + 2, little_endian)?;
            let count = read_u32(tiff, e + 4, little_endian)?;
            if field_type != TYPE_SHORT || count != 1 {
                return None;
            }
            // SHORT count=1: value lives in the first 2 bytes of the
            // 4-byte value field, interpreted per the TIFF byte order.
            let value = read_u16(tiff, e + 8, little_endian)?;
            return u8::try_from(value).ok();
        }
        None
    }

    fn read_u16(buf: &[u8], offset: usize, little_endian: bool) -> Option<u16> {
        let slice = buf.get(offset..offset + 2)?;
        let bytes = [slice[0], slice[1]];
        Some(if little_endian {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    }

    fn read_u32(buf: &[u8], offset: usize, little_endian: bool) -> Option<u32> {
        let slice = buf.get(offset..offset + 4)?;
        let bytes = [slice[0], slice[1], slice[2], slice[3]];
        Some(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
        read_u32(buf, offset, true)
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
    fn resize_image_returns_none_when_no_encoding_fits_budget() {
        // A real, decodable PNG against a 1-byte budget no encoding can
        // meet at any dimension down to 1x1, so the shrink loop bails
        // out with `None` (the caller then emits the omission note).
        let bytes = make_png(64, 64);
        let opts = ResizeOptions {
            max_bytes: 1,
            ..ResizeOptions::default()
        };
        assert!(resize_image(&bytes, "image/png", &opts).is_none());
    }

    #[test]
    fn resize_image_returns_none_for_undecodable_bytes() {
        assert!(resize_image(b"not an image", "image/png", &ResizeOptions::default()).is_none());
    }

    #[test]
    fn passthrough_image_decodes_dimensions_and_keeps_source_bytes() {
        let bytes = make_png(120, 80);
        let result = passthrough_image(&bytes, "image/png").expect("passthrough result");
        assert!(!result.was_resized);
        assert_eq!(result.original_width, 120);
        assert_eq!(result.original_height, 80);
        assert_eq!(result.width, 120);
        assert_eq!(result.height, 80);
        assert_eq!(result.mime_type, "image/png");
        // Passthrough forwards the source bytes verbatim, base64-encoded.
        assert_eq!(result.data, BASE64.encode(&bytes));
    }

    #[test]
    fn passthrough_image_returns_none_for_undecodable_bytes() {
        assert!(passthrough_image(b"not an image", "image/png").is_none());
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

    // -----------------------------------------------------------------
    // EXIF orientation
    // -----------------------------------------------------------------

    /// Build a minimal TIFF header carrying a single Orientation tag.
    /// `little_endian` toggles between II/MM byte order.
    fn build_tiff_orientation(orientation: u8, little_endian: bool) -> Vec<u8> {
        let mut t = Vec::new();
        if little_endian {
            t.extend_from_slice(b"II");
            t.extend_from_slice(&0x002Au16.to_le_bytes());
            t.extend_from_slice(&8u32.to_le_bytes());
            t.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
            t.extend_from_slice(&0x0112u16.to_le_bytes()); // tag
            t.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
            t.extend_from_slice(&1u32.to_le_bytes()); // count
            // SHORT value lives in the first 2 bytes of the value field
            // (little-endian: low byte first).
            t.extend_from_slice(&[orientation, 0, 0, 0]);
            t.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        } else {
            t.extend_from_slice(b"MM");
            t.extend_from_slice(&0x002Au16.to_be_bytes());
            t.extend_from_slice(&8u32.to_be_bytes());
            t.extend_from_slice(&1u16.to_be_bytes());
            t.extend_from_slice(&0x0112u16.to_be_bytes());
            t.extend_from_slice(&3u16.to_be_bytes());
            t.extend_from_slice(&1u32.to_be_bytes());
            // Big-endian SHORT: high byte first, so the meaningful
            // byte sits at offset+1 within the value field.
            t.extend_from_slice(&[0, orientation, 0, 0]);
            t.extend_from_slice(&0u32.to_be_bytes());
        }
        t
    }

    /// Wrap a TIFF blob in a JPEG APP1 segment and prepend an SOI.
    /// The trailing zero bytes are not meaningful — they keep the
    /// buffer non-empty after the segment so the segment walker can
    /// terminate cleanly.
    fn build_jpeg_with_exif(tiff: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        let mut app1 = vec![0xFF, 0xE1];
        let payload_len = u16::try_from(2 + 6 + tiff.len()).unwrap();
        app1.extend_from_slice(&payload_len.to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(tiff);
        out.extend(app1);
        // Trailing scan-like data — not parsed.
        out.extend_from_slice(&[0xFF, 0xDA, 0, 0]);
        out
    }

    /// Splice a minimal APP1 segment with the requested Orientation
    /// directly after the SOI of an existing JPEG byte stream.
    fn inject_exif_orientation(mut jpeg: Vec<u8>, orientation: u8) -> Vec<u8> {
        let tiff = build_tiff_orientation(orientation, false);
        let mut app1 = vec![0xFF, 0xE1];
        let payload_len = u16::try_from(2 + 6 + tiff.len()).unwrap();
        app1.extend_from_slice(&payload_len.to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut out: Vec<u8> = jpeg.drain(..2).collect();
        out.extend(app1);
        out.extend(jpeg);
        out
    }

    #[test]
    fn read_exif_orientation_returns_1_for_png() {
        let bytes = make_png(8, 8);
        assert_eq!(exif::read_exif_orientation(&bytes, "image/png"), 1);
    }

    #[test]
    fn read_exif_orientation_returns_1_for_jpeg_without_exif() {
        let bytes = jpeg_header();
        assert_eq!(exif::read_exif_orientation(&bytes, "image/jpeg"), 1);
    }

    #[test]
    fn read_exif_orientation_reads_jpeg_app1_orientation_6() {
        let tiff = build_tiff_orientation(6, false);
        let bytes = build_jpeg_with_exif(&tiff);
        assert_eq!(exif::read_exif_orientation(&bytes, "image/jpeg"), 6);
    }

    #[test]
    fn read_exif_orientation_handles_little_endian_jpeg() {
        let tiff = build_tiff_orientation(6, true);
        let bytes = build_jpeg_with_exif(&tiff);
        assert_eq!(exif::read_exif_orientation(&bytes, "image/jpeg"), 6);
    }

    #[test]
    fn apply_exif_orientation_identity() {
        let img = RgbaImage::from_pixel(40, 20, Rgba([10, 20, 30, 255]));
        let out = exif::apply_exif_orientation(img.clone(), 1);
        assert_eq!(out.dimensions(), (40, 20));
        assert_eq!(out.as_raw(), img.as_raw());
    }

    #[test]
    fn apply_exif_orientation_rotate90_swaps_dimensions() {
        let img = RgbaImage::from_pixel(100, 50, Rgba([10, 20, 30, 255]));
        let out = exif::apply_exif_orientation(img, 6);
        assert_eq!(out.dimensions(), (50, 100));
    }

    #[test]
    fn apply_exif_orientation_rotate180_preserves_dimensions() {
        let mut img = RgbaImage::from_pixel(100, 50, Rgba([0, 0, 0, 255]));
        // Mark the top-left corner with a distinctive color.
        img.put_pixel(0, 0, Rgba([255, 7, 11, 255]));
        let out = exif::apply_exif_orientation(img, 3);
        assert_eq!(out.dimensions(), (100, 50));
        // After 180° rotation the marked pixel lands at the
        // diagonally opposite corner.
        assert_eq!(*out.get_pixel(99, 49), Rgba([255, 7, 11, 255]));
        assert_eq!(*out.get_pixel(0, 0), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn resize_image_corrects_jpeg_orientation_6() {
        // 100×50 RGBA → JPEG → inject APP1(Orientation=6).
        let mut img = RgbaImage::new(100, 50);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = u8::try_from((x ^ y) & 0xff).unwrap_or(0);
            *px = Rgba([r, 64, 200, 255]);
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .to_rgb8()
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .expect("encode jpeg");
        let bytes = inject_exif_orientation(jpeg, 6);

        assert_eq!(exif::read_exif_orientation(&bytes, "image/jpeg"), 6);

        let resized =
            resize_image(&bytes, "image/jpeg", &ResizeOptions::default()).expect("resize result");
        // Orientation 6 swaps width/height; corrected dimensions
        // should be reported as (50, 100), not the sensor (100, 50).
        assert_eq!(resized.original_width, 50);
        assert_eq!(resized.original_height, 100);
    }
}
