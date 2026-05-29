//! System-clipboard image paste support.
//!
//! Terminals deliver bracketed paste as text, but an image copied from
//! another app usually has no textual payload. For paste-key chords we
//! therefore query the OS clipboard directly, save any image as a PNG,
//! and let the normal text-input paste path insert the resulting path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::input::KeyMessage;

static PASTE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum ClipboardImageError {
    #[error("clipboard init failed: {0}")]
    ClipboardInit(arboard::Error),

    #[error("clipboard image read failed: {0}")]
    ClipboardRead(arboard::Error),

    #[error("clipboard image has invalid RGBA buffer length: {width}x{height}, {bytes} bytes")]
    InvalidImageBuffer {
        width: usize,
        height: usize,
        bytes: usize,
    },

    #[error("failed to create clipboard image directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to encode clipboard image as PNG: {0}")]
    Encode(image::ImageError),

    #[error("failed to write clipboard image {path}: {source}")]
    Write {
        path: PathBuf,
        source: image::ImageError,
    },
}

/// Paste-key chords where terminal text paste has no useful payload if
/// the clipboard currently contains an image.
pub fn is_clipboard_image_paste_key(key: &KeyMessage) -> bool {
    if !key.name.eq_ignore_ascii_case("v") {
        return false;
    }
    let has_ctrl = key.mods.contains(&"ctrl");
    let has_super = key.mods.contains(&"super");
    let has_alt = key.mods.contains(&"alt");
    (has_ctrl || has_super) && !has_alt
}

/// Save the current system clipboard image, if there is one, and return
/// the absolute PNG path to insert into chat input.
pub fn save_system_clipboard_image() -> Result<Option<PathBuf>, ClipboardImageError> {
    let mut clipboard = arboard::Clipboard::new().map_err(ClipboardImageError::ClipboardInit)?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(ClipboardImageError::ClipboardRead(e)),
    };
    let dir = clipboard_image_dir();
    let path = next_clipboard_image_path(&dir);
    save_rgba_png(&path, image.width, image.height, image.bytes.as_ref())?;
    Ok(Some(path))
}

fn clipboard_image_dir() -> PathBuf {
    std::env::var_os("NEFOR_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("nefor"))
        .join("clipboard-images")
}

fn next_clipboard_image_path(dir: &Path) -> PathBuf {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = PASTE_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("clipboard-image-{ms}-{pid}-{n}.png"))
}

fn save_rgba_png(
    path: &Path,
    width: usize,
    height: usize,
    bytes: &[u8],
) -> Result<(), ClipboardImageError> {
    let expected = width.saturating_mul(height).saturating_mul(4);
    if bytes.len() != expected {
        return Err(ClipboardImageError::InvalidImageBuffer {
            width,
            height,
            bytes: bytes.len(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ClipboardImageError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    image::save_buffer_with_format(
        path,
        bytes,
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|source| ClipboardImageError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_key_accepts_ctrl_or_super_v() {
        assert!(is_clipboard_image_paste_key(&KeyMessage {
            name: "v".into(),
            mods: vec!["ctrl"],
        }));
        assert!(is_clipboard_image_paste_key(&KeyMessage {
            name: "v".into(),
            mods: vec!["super"],
        }));
        assert!(!is_clipboard_image_paste_key(&KeyMessage {
            name: "v".into(),
            mods: vec![],
        }));
        assert!(!is_clipboard_image_paste_key(&KeyMessage {
            name: "v".into(),
            mods: vec!["alt"],
        }));
    }

    #[test]
    fn save_rgba_png_writes_png_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paste.png");
        save_rgba_png(&path, 1, 1, &[255, 0, 0, 255]).expect("save png");
        let bytes = std::fs::read(&path).expect("read png");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn save_rgba_png_rejects_bad_buffer_len() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paste.png");
        let err = save_rgba_png(&path, 2, 1, &[255, 0, 0, 255]).unwrap_err();
        assert!(matches!(
            err,
            ClipboardImageError::InvalidImageBuffer {
                width: 2,
                height: 1,
                bytes: 4
            }
        ));
    }
}
