//! `read_image` — read image bytes for vision-capable providers.
//!
//! The tool loads and classifies bytes. It does not OCR or caption;
//! interpretation belongs to the model layer. Oversized images are
//! downscaled/re-encoded so clipboard screenshots don't bounce off model
//! payload limits. Providers that cannot send image parts must turn the
//! structured media result into an explicit user-visible error before
//! the next model turn.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::error::ToolError;

/// Wire name for this tool.
pub const NAME: &str = "read_image";

/// Human-readable description shipped to the LLM via the provider.
pub const DESCRIPTION: &str =
    "Read an image file for visual inspection. Returns image bytes and metadata; only vision-capable models can use the result.";

/// Hard cap on the source image read. Larger files are probably not a
/// pasted screenshot and should fail instead of loading into memory.
pub const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;

/// Target cap for the media bytes returned to the provider. Larger
/// source images are downscaled/re-encoded before base64 wrapping.
pub const TARGET_OUTPUT_BYTES: usize = 5 * 1024 * 1024;

/// JSON Schema (OpenAI tool-call format) for `read_image`'s parameters.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute or relative path to the image file."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Relative paths are resolved against this."
            }
        },
        "required": ["path"]
    })
}

/// Execute `read_image` with the given args.
pub async fn run(args: &Value) -> Result<Value, ToolError> {
    let request = parse_args(args)?;
    read_image_file(request).await
}

#[derive(Debug)]
struct ReadImageRequest {
    path: String,
}

fn parse_args(args: &Value) -> Result<ReadImageRequest, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;
    let raw = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `path`".into(),
        })?;
    if raw.is_empty() {
        return Err(ToolError::BadArgs {
            tool: NAME.into(),
            message: "`path` must be non-empty".into(),
        });
    }
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    Ok(ReadImageRequest {
        path: resolve_path(raw, cwd),
    })
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_owned();
    }
    match cwd {
        Some(dir) => std::path::Path::new(dir)
            .join(p)
            .to_string_lossy()
            .into_owned(),
        None => path.to_owned(),
    }
}

async fn read_image_file(request: ReadImageRequest) -> Result<Value, ToolError> {
    let path = request.path;
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::NotFound { path });
        }
        Err(e) => {
            return Err(ToolError::Io {
                path,
                message: e.to_string(),
            });
        }
    };

    if meta.is_dir() {
        return Err(ToolError::IsDirectory { path });
    }

    let size = meta.len();
    if size > MAX_INPUT_BYTES {
        return Err(ToolError::ImageTooLarge {
            size,
            cap: MAX_INPUT_BYTES,
            path,
        });
    }

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(ToolError::ImageTooLarge {
            size: bytes.len() as u64,
            cap: MAX_INPUT_BYTES,
            path,
        });
    }

    let media_type = detect_image_mime(&bytes)
        .ok_or_else(|| ToolError::UnsupportedImage { path: path.clone() })?;
    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&path)
        .to_owned();

    let media = prepare_media_bytes(&bytes, media_type)
        .map_err(|_| ToolError::UnsupportedImage { path: path.clone() })?;

    let debug_path = maybe_write_debug_media(&filename, &media).await?;
    let mut out = json!({
        "type": "media",
        "media_type": media.media_type,
        "filename": filename,
        "data": encode_base64(&media.bytes),
    });
    if let Some(debug_path) = debug_path {
        out["debug_path"] = Value::String(debug_path);
    }
    Ok(out)
}

struct MediaBytes {
    media_type: &'static str,
    bytes: Vec<u8>,
}

fn prepare_media_bytes(
    bytes: &[u8],
    media_type: &'static str,
) -> Result<MediaBytes, image::ImageError> {
    if bytes.len() <= TARGET_OUTPUT_BYTES {
        return Ok(MediaBytes {
            media_type,
            bytes: bytes.to_vec(),
        });
    }

    let img = image::load_from_memory(bytes)?;
    let variants = compressed_variants(&img);
    for encoded in variants.iter().rev() {
        if encoded.len() <= TARGET_OUTPUT_BYTES {
            return Ok(MediaBytes {
                media_type: "image/jpeg",
                bytes: encoded.clone(),
            });
        }
    }

    Ok(MediaBytes {
        media_type: "image/jpeg",
        bytes: variants
            .into_iter()
            .next()
            .unwrap_or_else(|| bytes.to_vec()),
    })
}

fn compressed_variants(img: &DynamicImage) -> Vec<Vec<u8>> {
    const MAX_EDGES: [u32; 7] = [2048, 1600, 1280, 1024, 768, 512, 384];
    const QUALITIES: [u8; 6] = [85, 75, 65, 55, 45, 35];

    let mut out = Vec::new();
    for max_edge in MAX_EDGES {
        let resized = resize_to_max_edge(img, max_edge);
        let rgb = flatten_alpha(&resized);
        for quality in QUALITIES {
            if let Ok(bytes) = encode_jpeg(&rgb, quality) {
                out.push(bytes);
            }
        }
    }
    out.sort_by_key(Vec::len);
    out
}

fn resize_to_max_edge(img: &DynamicImage, max_edge: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest <= max_edge {
        return img.clone();
    }
    let ratio = max_edge as f32 / longest as f32;
    let nw = ((w as f32 * ratio).round() as u32).max(1);
    let nh = ((h as f32 * ratio).round() as u32).max(1);
    img.resize(nw, nh, FilterType::Triangle)
}

fn flatten_alpha(img: &DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y);
        let alpha = p[3] as u16;
        let inv = 255u16.saturating_sub(alpha);
        image::Rgb([
            ((p[0] as u16 * alpha + 255 * inv) / 255) as u8,
            ((p[1] as u16 * alpha + 255 * inv) / 255) as u8,
            ((p[2] as u16 * alpha + 255 * inv) / 255) as u8,
        ])
    })
}

fn encode_jpeg(img: &image::RgbImage, quality: u8) -> Result<Vec<u8>, image::ImageError> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder.encode(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

async fn maybe_write_debug_media(
    filename: &str,
    media: &MediaBytes,
) -> Result<Option<String>, ToolError> {
    let Some(dir) = std::env::var_os("NEFOR_READ_IMAGE_DEBUG_DIR") else {
        return Ok(None);
    };
    let dir = std::path::PathBuf::from(dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ToolError::Io {
            path: dir.to_string_lossy().into_owned(),
            message: e.to_string(),
        })?;

    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("image");
    let safe_stem = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!(
        "{safe_stem}-{millis}.{}",
        extension_for_media_type(media.media_type)
    ));

    tokio::fs::write(&path, &media.bytes)
        .await
        .map_err(|e| ToolError::Io {
            path: path.to_string_lossy().into_owned(),
            message: e.to_string(),
        })?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn extension_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "img",
    }
}

fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn reads_png_as_media_object() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"\x89PNG\r\n\x1a\nabc").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let out = run(&json!({ "path": path })).await.expect("ok");
        assert_eq!(out.get("type").and_then(Value::as_str), Some("media"));
        assert_eq!(
            out.get("media_type").and_then(Value::as_str),
            Some("image/png")
        );
        assert_eq!(
            out.get("data").and_then(Value::as_str),
            Some("iVBORw0KGgphYmM=")
        );
    }

    #[tokio::test]
    async fn rejects_unsupported_image_format() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"not an image").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        assert!(
            matches!(err, ToolError::UnsupportedImage { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.gif");
        std::fs::write(&path, b"GIF89a").expect("write");
        let out = run(&json!({
            "path": "image.gif",
            "cwd": dir.path().to_str().expect("utf8 cwd")
        }))
        .await
        .expect("ok");
        assert_eq!(
            out.get("media_type").and_then(Value::as_str),
            Some("image/gif")
        );
    }

    #[test]
    fn schema_has_required_path() {
        let s = schema();
        assert_eq!(s.get("type").and_then(Value::as_str), Some("object"));
        let required = s
            .get("required")
            .and_then(Value::as_array)
            .expect("required");
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
    }

    #[test]
    fn large_png_is_reencoded_under_target_cap() {
        let width = 2048u32;
        let height = 2048u32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        let mut seed = 0x1234_5678u32;
        for _ in 0..(width * height) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            rgba.push((seed >> 24) as u8);
            rgba.push((seed >> 16) as u8);
            rgba.push((seed >> 8) as u8);
            rgba.push(255);
        }

        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .expect("encode png");
        assert!(
            png.len() > TARGET_OUTPUT_BYTES,
            "fixture should exceed target cap, got {}",
            png.len()
        );

        let media = prepare_media_bytes(&png, "image/png").expect("prepare media");
        assert_eq!(media.media_type, "image/jpeg");
        assert!(
            media.bytes.len() <= TARGET_OUTPUT_BYTES,
            "re-encoded media should be under target cap, got {}",
            media.bytes.len()
        );
        assert!(media.bytes.starts_with(&[0xff, 0xd8, 0xff]));
    }
}
