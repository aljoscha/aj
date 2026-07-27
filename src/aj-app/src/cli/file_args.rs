//! Resolution of `@file` arguments into prompt content.
//!
//! The CLI treats any positional argument beginning with `@` as a file
//! attachment. Each path is resolved relative to the process working
//! directory after stripping one leading `@`, then:
//!
//! - Text files become a `<file name="ABS">…</file>` block appended to
//!   the combined prompt text, so the model sees the contents with
//!   clear provenance.
//! - Image files (detected by content sniffing) are resized under the
//!   inline-image budget and attached as [`UserContent::Image`] blocks,
//!   with a `<file name="ABS">…</file>` text reference carrying the
//!   dimension note. An image that can't be shrunk under the budget is
//!   referenced with an `[Image omitted: …]` note and no attachment.
//!
//! Resolution is strict: a missing file is a hard error (the caller
//! aborts before starting a turn), mirroring the one-shot nature of a
//! launch prompt. Empty files are skipped.

use std::path::Path;

use anyhow::{Context, Result};

use aj_models::types::UserContent;
use aj_tools::image::{self, ResizeOptions};
use aj_tools::path::resolve_read_path;

/// Outcome of resolving a batch of `@file` arguments.
#[derive(Debug)]
pub struct ResolvedFiles {
    /// Concatenated `<file name="…">…</file>` blocks, in argument
    /// order. Empty when no file arguments were supplied (or all were
    /// empty files).
    pub text: String,
    /// Image attachments resolved from image file arguments, in
    /// argument order.
    pub images: Vec<UserContent>,
}

/// Resolve `@file` arguments into prompt text plus image attachments,
/// relative to `cwd`.
///
/// Returns an error on the first missing or unreadable file.
pub fn process_file_args(
    file_args: &[String],
    cwd: &Path,
    image_auto_resize: bool,
) -> Result<ResolvedFiles> {
    let mut text = String::new();
    let mut images = Vec::new();

    for arg in file_args {
        let path = resolve_read_path(arg, cwd);
        let display = path.display().to_string();

        let metadata =
            std::fs::metadata(&path).with_context(|| format!("@file not found: {display}"))?;
        // Skip empty files: they contribute neither useful text nor a
        // decodable image, and an empty `<file>` block is just noise.
        if metadata.len() == 0 {
            continue;
        }

        match image::detect_mime_type_from_file(&path) {
            Some(mime) => append_image(
                &path,
                &display,
                mime,
                image_auto_resize,
                &mut text,
                &mut images,
            )?,
            None => {
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("could not read @file as text: {display}"))?;
                text.push_str(&format!("<file name=\"{display}\">\n{content}\n</file>\n"));
            }
        }
    }

    Ok(ResolvedFiles { text, images })
}

/// Read, resize, and attach an image file. The resized payload rides on
/// `images` as a [`UserContent::Image`]; a `<file>` text reference
/// (carrying the dimension note when the image was scaled) is appended
/// to `text` so the model can correlate the attachment with its path.
fn append_image(
    path: &Path,
    display: &str,
    mime: &'static str,
    auto_resize: bool,
    text: &mut String,
    images: &mut Vec<UserContent>,
) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("could not read @file: {display}"))?;
    let processed = if auto_resize {
        image::resize_image(&bytes, mime, &ResizeOptions::default())
    } else {
        image::passthrough_image(&bytes, mime)
    };
    match processed {
        Some(resized) => {
            let note = image::format_dimension_note(&resized).unwrap_or_default();
            text.push_str(&format!("<file name=\"{display}\">{note}</file>\n"));
            images.push(UserContent::image(resized.data, resized.mime_type));
        }
        None => {
            text.push_str(&format!(
                "<file name=\"{display}\">[Image omitted: could not be resized below the inline image size limit.]</file>\n"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use aj_models::types::UserContent;
    use tempfile::tempdir;

    use crate::cli::file_args::process_file_args;

    #[test]
    fn wraps_text_file_in_file_tag() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello world").expect("write");

        let resolved =
            process_file_args(&[file.display().to_string()], dir.path(), true).expect("resolve");
        assert!(resolved.images.is_empty());
        assert_eq!(
            resolved.text,
            format!("<file name=\"{}\">\nhello world\n</file>\n", file.display())
        );
    }

    #[test]
    fn resolves_relative_to_cwd() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("rel.txt"), "x").expect("write");

        let resolved =
            process_file_args(&["rel.txt".to_string()], dir.path(), true).expect("resolve");
        assert!(resolved.text.contains("rel.txt"));
        assert!(resolved.text.contains(">\nx\n<"));
    }

    #[test]
    fn skips_empty_files() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("empty.txt");
        std::fs::File::create(&file).expect("create");

        let resolved =
            process_file_args(&[file.display().to_string()], dir.path(), true).expect("resolve");
        assert!(resolved.text.is_empty());
        assert!(resolved.images.is_empty());
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempdir().expect("tempdir");
        let err = process_file_args(&["does-not-exist.txt".to_string()], dir.path(), true)
            .expect_err("should error");
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn attaches_image_as_content_block() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("pixel.png");
        let mut handle = std::fs::File::create(&file).expect("create");
        handle.write_all(&tiny_png()).expect("write");
        handle.flush().expect("flush");

        let resolved =
            process_file_args(&[file.display().to_string()], dir.path(), true).expect("resolve");
        assert_eq!(resolved.images.len(), 1);
        // A small image fits the budget unscaled, so no dimension note.
        assert_eq!(
            resolved.text,
            format!("<file name=\"{}\"></file>\n", file.display())
        );
    }

    /// A real 1x1 PNG so MIME sniffing and decoding both succeed.
    fn tiny_png() -> Vec<u8> {
        use image::{ImageFormat, Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .expect("encode png");
        buf
    }

    #[test]
    fn disabled_auto_resize_attaches_original_image() {
        use base64::Engine;
        use image::{ImageFormat, Rgba, RgbaImage};

        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("wide.png");
        let image = RgbaImage::from_pixel(2001, 1, Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png");
        std::fs::write(&file, &bytes).expect("write");

        let resolved =
            process_file_args(&[file.display().to_string()], dir.path(), false).expect("resolve");
        let UserContent::Image(attached) = &resolved.images[0] else {
            panic!("expected image attachment");
        };
        assert_eq!(
            attached.data,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert!(!resolved.text.contains("Multiply coordinates"));
    }
}
