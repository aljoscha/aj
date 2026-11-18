//! Client-local image references and preparation of editor prompts.
//!
//! Only explicitly attached files become image content. Ordinary paths in prose
//! remain text. Preparation snapshots the bytes before any host operation, so
//! remote hosts never need to open a client path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aj_models::types::UserContent;
use aj_tools::image::{self, ResizeOptions};
use anyhow::{Context, Result, bail};
use base64::Engine;

/// A prepared submission plus the editable draft to restore on refusal.
#[derive(Clone, Debug)]
pub struct PreparedPrompt {
    pub draft: String,
    pub content: Vec<UserContent>,
}

impl From<String> for PreparedPrompt {
    fn from(draft: String) -> Self {
        Self {
            content: vec![UserContent::text(&draft)],
            draft,
        }
    }
}

/// References live as long as the client's shared editor history. Deleting a
/// marker removes it from a submission, not from undo or history. Random IDs
/// prevent ordinary numbered-marker text in recalled prompts from acquiring
/// a binding to an unrelated image, including after a client restart.
#[derive(Clone, Default)]
pub struct ImageAttachments {
    paths: BTreeMap<String, PathBuf>,
}

impl ImageAttachments {
    pub fn attach(&mut self, path: PathBuf) -> String {
        let marker = loop {
            let id =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 12]>());
            let marker = format!("[Image #{id}]");
            if !self.paths.contains_key(&marker) {
                break marker;
            }
        };
        self.paths.insert(marker.clone(), path);
        marker
    }

    pub fn markers(&self) -> impl Iterator<Item = &str> {
        self.paths.keys().map(String::as_str)
    }

    pub fn contains_images(&self, text: &str) -> bool {
        self.markers().any(|marker| text.contains(marker))
    }

    /// Resolve only markers still present in the draft. Any preparation error
    /// aborts the entire submission. No partial prompt or omitted image is sent.
    pub fn prepare(&self, draft: &str, auto_resize: bool) -> Result<PreparedPrompt> {
        let draft = draft.trim().to_string();
        let mut remaining = draft.as_str();
        let mut content = Vec::new();
        while let Some((offset, marker, path)) = self
            .paths
            .iter()
            .filter_map(|(marker, path)| {
                remaining.find(marker).map(|offset| (offset, marker, path))
            })
            .min_by_key(|(offset, _, _)| *offset)
        {
            if offset > 0 {
                content.push(UserContent::text(&remaining[..offset]));
            }
            let image = prepare_image(path, auto_resize)
                .with_context(|| format!("Could not attach {}", path.display()))?;
            // Include provenance without persisting a live editor token. Prompt
            // history loaded in another process carries provenance, not an
            // editor token whose backing reference only lives in this client.
            let mut note = format!("[Attached image: {}]", path.display());
            if let Some(dimensions) = image::format_dimension_note(&image) {
                note.push('\n');
                note.push_str(&dimensions);
            }
            content.push(UserContent::text(note));
            content.push(UserContent::image(image.data, image.mime_type));
            remaining = &remaining[offset + marker.len()..];
        }
        if !remaining.is_empty() {
            content.push(UserContent::text(remaining));
        }
        Ok(PreparedPrompt { draft, content })
    }
}

fn prepare_image(path: &Path, auto_resize: bool) -> Result<image::ResizedImage> {
    let bytes = std::fs::read(path)?;
    let Some(mime) = image::detect_mime_type(&bytes) else {
        bail!("not a supported image (PNG, JPEG, GIF or WebP)");
    };
    let result = if auto_resize {
        image::resize_image(&bytes, mime, &ResizeOptions::default())
    } else {
        image::passthrough_image(&bytes, mime)
    };
    result.context("image could not be decoded or prepared within the image size limit")
}

/// Recognize a terminal file drop, delivered as pasted text. The entire paste
/// must resolve to local image files. Prose, mixed file types and web URLs remain
/// ordinary text. Try a literal path first for terminals that do not quote
/// spaces, then shell-quoted/escaped paths (without executing any shell code).
pub fn dropped_images(text: &str, cwd: &Path) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.contains('\0') {
        return None;
    }
    let resolve = |input: &str| {
        let path = aj_tools::path::resolve_read_path(input, cwd);
        (path.is_file() && image::detect_mime_type_from_file(&path).is_some()).then_some(path)
    };
    if let Some(path) = resolve(text) {
        return Some(vec![path]);
    }
    let words = shlex::split(text)?;
    if words.is_empty() {
        return None;
    }
    words.iter().map(|word| resolve(word)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn image_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        ::image::DynamicImage::new_rgb8(3, 2).save(&path).unwrap();
        path
    }

    #[test]
    fn drops_accept_literal_quoted_escaped_uri_and_multiple_paths_not_prose() {
        let dir = tempfile::tempdir().unwrap();
        let first = image_file(dir.path(), "first image.png");
        let second = image_file(dir.path(), "second.png");
        for text in [
            first.display().to_string(),
            format!("'{}'", first.display()),
            format!("\"{}\"", first.display()),
            first.display().to_string().replace(' ', "\\ "),
            url::Url::from_file_path(&first).unwrap().to_string(),
            "'first image.png'".into(),
        ] {
            assert_eq!(dropped_images(&text, dir.path()), Some(vec![first.clone()]));
        }
        assert_eq!(
            dropped_images("'first image.png' second.png", dir.path()),
            Some(vec![first, second])
        );
        for text in [
            "explain second.png",
            "missing.png",
            "https://example.com/x.png",
            "'second.png",
            "",
            "hello",
        ] {
            assert!(dropped_images(text, dir.path()).is_none(), "{text}");
        }
    }

    #[test]
    fn preparation_snapshots_images_in_draft_order_and_ignores_deleted_markers() {
        let dir = tempfile::tempdir().unwrap();
        let first = image_file(dir.path(), "first.png");
        let second = image_file(dir.path(), "second.jpg");
        let mut attachments = ImageAttachments::default();
        let a = attachments.attach(first.clone());
        let b = attachments.attach(second.clone());
        let removed = attachments.attach(dir.path().join("deleted.png"));
        let draft = format!("Compare {b} with {a}");
        let prepared = attachments.prepare(&draft, true).unwrap();
        assert_eq!(prepared.draft, draft);
        assert!(!prepared.draft.contains(&removed));
        let images: Vec<_> = prepared
            .content
            .iter()
            .filter_map(|block| match block {
                UserContent::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 2);
        for (image, path) in images.iter().zip([&second, &first]) {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .unwrap(),
                std::fs::read(path).unwrap()
            );
        }
        std::fs::remove_file(second).unwrap();
        assert!(attachments.prepare(&draft, true).is_err());
        // The prepared prompt is self-contained even after local files vanish.
        assert_eq!(images[0].mime_type, "image/jpeg");
        let persisted_text = prepared
            .content
            .iter()
            .filter_map(|block| match block {
                UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(!attachments.contains_images(&persisted_text));
    }

    #[test]
    fn literal_numbered_labels_and_old_client_markers_do_not_bind_to_new_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = image_file(dir.path(), "image.png");
        let mut old_client = ImageAttachments::default();
        let old_marker = old_client.attach(path.clone());
        let mut new_client = ImageAttachments::default();
        let new_marker = new_client.attach(path);
        assert_ne!(old_marker, new_marker);
        for text in ["Explain [Image #1]", old_marker.as_str()] {
            assert!(!new_client.contains_images(text));
            let prepared = new_client.prepare(text, true).unwrap();
            assert_eq!(prepared.content.len(), 1);
            assert!(matches!(&prepared.content[0], UserContent::Text(block) if block.text == text));
        }
    }

    #[test]
    fn invalid_images_fail_without_consuming_attachment_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let mut attachments = ImageAttachments::default();
        let marker = attachments.attach(path.clone());
        std::fs::write(&path, "not an image").unwrap();
        assert!(attachments.prepare(&marker, true).is_err());
        image_file(dir.path(), "image.png");
        assert!(attachments.prepare(&marker, true).is_ok());
        assert_eq!(
            attachments
                .prepare("ordinary text", true)
                .unwrap()
                .content
                .len(),
            1
        );
    }
}
