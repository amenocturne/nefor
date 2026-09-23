mod error {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/error.rs"));
}

mod tools {

    pub mod process {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/process.rs"));
    }

    pub mod process_exec {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/process_exec.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;

            fn unbounded() -> Value {
                json!({ "present": false, "milliseconds": 0 })
            }

            #[tokio::test]
            async fn rejects_empty_argv() {
                assert!(matches!(
                    run(&json!({"argv": [], "cwd": "/", "timeout": unbounded()})).await,
                    Err(ToolError::BadArgs { .. })
                ));
            }
        }
    }

    pub mod read_file {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/read_file.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;
            use std::io::Write;
            use tempfile::NamedTempFile;

            #[cfg(unix)]
            use std::ffi::CString;
            #[cfg(unix)]
            use std::fs::OpenOptions;
            #[cfg(unix)]
            use std::os::unix::ffi::OsStrExt;
            #[cfg(unix)]
            use std::os::unix::net::UnixListener;
            #[cfg(unix)]
            use std::path::Path;
            #[cfg(unix)]
            use std::time::Duration;

            const TEST_MAX_BYTES: u64 = 1024 * 1024;

            #[cfg(unix)]
            fn create_fifo(path: &Path) {
                let path = CString::new(path.as_os_str().as_bytes()).expect("fifo path");
                let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
                assert_eq!(
                    result,
                    0,
                    "mkfifo failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            #[cfg(unix)]
            async fn assert_not_regular(path: &Path) {
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    run(
                        &json!({ "path": path.to_str().expect("utf8 path") }),
                        Some(TEST_MAX_BYTES),
                    ),
                )
                .await
                .expect("read_file must reject before a special file can block")
                .unwrap_err();
                assert!(
                    matches!(result, ToolError::NotRegularFile { .. }),
                    "got {result:?}"
                );
            }

            #[tokio::test]
            async fn reads_utf8_contents() {
                let mut f = NamedTempFile::new().expect("tempfile");
                write!(f, "hello world").expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let out = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .expect("ok");
                assert_eq!(out, "hello world");
            }

            #[tokio::test]
            async fn reads_empty_file() {
                let f = NamedTempFile::new().expect("tempfile");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let out = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .expect("ok");
                assert_eq!(out, "");
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn reads_symlink_to_regular_file() {
                let mut file = NamedTempFile::new().expect("tempfile");
                file.write_all(b"linked text").expect("write");
                let dir = tempfile::tempdir().expect("tempdir");
                let link = dir.path().join("text-link");
                std::os::unix::fs::symlink(file.path(), &link).expect("symlink");

                let out = run(
                    &json!({ "path": link.to_str().expect("utf8 path") }),
                    Some(TEST_MAX_BYTES),
                )
                .await
                .expect("symlink to regular file accepted");
                assert_eq!(out, "linked text");
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn rejects_device_and_symlink_to_device() {
                for device in ["/dev/null", "/dev/random", "/dev/urandom"] {
                    assert_not_regular(Path::new(device)).await;
                }

                let dir = tempfile::tempdir().expect("tempdir");
                let link = dir.path().join("device-link");
                std::os::unix::fs::symlink("/dev/null", &link).expect("symlink");
                assert_not_regular(&link).await;
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn rejects_fifo_before_opening_it() {
                let dir = tempfile::tempdir().expect("tempdir");
                let fifo = dir.path().join("pipe");
                create_fifo(&fifo);
                let _guard = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&fifo)
                    .expect("open fifo guard");

                assert_not_regular(&fifo).await;
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn rejects_unix_socket() {
                let dir = tempfile::tempdir().expect("tempdir");
                let socket = dir.path().join("socket");
                let _listener = UnixListener::bind(&socket).expect("bind unix socket");

                assert_not_regular(&socket).await;
            }

            #[tokio::test]
            async fn rejects_missing_path() {
                let err = run(
                    &json!({ "path": "/definitely/does/not/exist/abcxyz" }),
                    Some(1024 * 1024),
                )
                .await
                .unwrap_err();
                assert!(matches!(err, ToolError::NotFound { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn rejects_directory() {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().to_str().expect("utf8 path").to_owned();
                let err = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::IsDirectory { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn rejects_binary_content() {
                let mut f = NamedTempFile::new().expect("tempfile");
                // Write a NUL in the first 8 KiB.
                f.write_all(b"hello\0world").expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let err = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                assert!(
                    matches!(err, ToolError::BinaryContent { .. }),
                    "got {err:?}"
                );
            }

            #[tokio::test]
            async fn rejects_too_large() {
                let mut f = NamedTempFile::new().expect("tempfile");
                // 1 MiB + 1 byte of ASCII 'a'.
                let big = vec![b'a'; (TEST_MAX_BYTES as usize) + 1];
                f.write_all(&big).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let err = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                match err {
                    ToolError::TooLarge { size, .. } => {
                        assert_eq!(size, TEST_MAX_BYTES + 1);
                    }
                    other => panic!("expected TooLarge, got {other:?}"),
                }
            }

            #[tokio::test]
            async fn accepts_exactly_max_bytes() {
                let mut f = NamedTempFile::new().expect("tempfile");
                let buf = vec![b'a'; TEST_MAX_BYTES as usize];
                f.write_all(&buf).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let out = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .expect("ok");
                assert_eq!(out.len(), TEST_MAX_BYTES as usize);
            }

            #[tokio::test]
            async fn sliced_read_allows_large_file_in_bounded_chunks() {
                let mut f = NamedTempFile::new().expect("tempfile");
                let big = format!("{}END", "a".repeat((TEST_MAX_BYTES as usize) + 10));
                f.write_all(big.as_bytes()).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();

                let out = run(
                    &json!({
                        "path": path,
                        "offset": TEST_MAX_BYTES + 5,
                        "max_bytes": 16
                    }),
                    Some(1024 * 1024),
                )
                .await
                .expect("slice ok");

                assert!(out.contains("[read_file slice: bytes "));
                assert!(out.contains("aaaaaEND"));
                assert!(!out.contains("file too large"));
            }

            #[tokio::test]
            async fn sliced_read_reports_next_offset_when_file_continues() {
                let mut f = NamedTempFile::new().expect("tempfile");
                f.write_all(b"0123456789abcdef").expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();

                let out = run(
                    &json!({
                        "path": path,
                        "offset": 2,
                        "max_bytes": 5
                    }),
                    Some(1024 * 1024),
                )
                .await
                .expect("slice ok");

                assert!(out.contains("[read_file slice: bytes 2..7 of 16]"));
                assert!(out.contains("23456"));
                assert!(out.contains("next offset: 7"));
            }

            #[tokio::test]
            async fn sliced_read_does_not_split_utf8_at_boundaries() {
                let mut f = NamedTempFile::new().expect("tempfile");
                f.write_all("aa€b".as_bytes()).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();

                let first = run(
                    &json!({
                        "path": path,
                        "offset": 0,
                        "max_bytes": 4
                    }),
                    Some(1024 * 1024),
                )
                .await
                .expect("first slice ok");
                assert!(first.contains("\naa"));
                assert!(first.contains("next offset: 2"));

                let second = run(
                    &json!({
                        "path": path,
                        "offset": 3,
                        "max_bytes": 4
                    }),
                    Some(1024 * 1024),
                )
                .await
                .expect("second slice ok");
                assert!(second.contains("[read_file slice: bytes 5..6 of 6]"));
                assert!(second.contains("\nb"));
            }

            #[tokio::test]
            async fn rejects_invalid_utf8() {
                let mut f = NamedTempFile::new().expect("tempfile");
                // Valid-looking ASCII followed by a stray UTF-8 continuation byte.
                // No NUL so it doesn't trip the binary heuristic.
                f.write_all(&[b'h', b'i', 0xC3, 0x28]).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();
                let err = run(&json!({ "path": path }), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::NotUtf8 { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn rejects_bad_args_no_path() {
                let err = run(&json!({}), Some(1024 * 1024)).await.unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn rejects_bad_args_empty_path() {
                let err = run(&json!({ "path": "" }), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn rejects_bad_args_non_object() {
                let err = run(&json!("just a string"), Some(1024 * 1024))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
            }

            #[tokio::test]
            async fn configured_boundary_accepts_limit_rejects_above_and_reads_offset() {
                let mut f = NamedTempFile::new().expect("tempfile");
                f.write_all(&vec![b'x'; 40000]).expect("write");
                let path = f.path().to_str().expect("utf8 path").to_owned();

                let exact = run(
                    &json!({
                        "path": path,
                        "offset": 1000,
                        "max_bytes": 32768
                    }),
                    Some(32768),
                )
                .await
                .expect("exact configured maximum accepted");
                assert!(exact.contains("[read_file slice: bytes 1000..33768 of 40000]"));
                assert!(exact.contains(&"x".repeat(32768)));

                let err = run(
                    &json!({
                        "path": path,
                        "offset": 1000,
                        "max_bytes": 32769
                    }),
                    Some(32768),
                )
                .await
                .unwrap_err();
                assert!(err.to_string().contains(
                    "`max_bytes` must not exceed the configured maximum of 32768 bytes (requested 32769)"
                ), "got {err}");
            }

            #[test]
            fn display_preview_remains_twenty_lines_and_1600_bytes() {
                let result = display()["result"]["fields"][0].clone();
                assert_eq!(result["max_lines"], 20);
                assert_eq!(result["max_bytes"], 1600);
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
        }
    }

    pub mod read_image {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/read_image.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;
            use image::ImageEncoder;
            use std::io::Write;
            use tempfile::NamedTempFile;

            #[cfg(unix)]
            use std::os::unix::net::UnixListener;
            #[cfg(unix)]
            use std::path::Path;
            #[cfg(unix)]
            use std::time::Duration;

            #[cfg(unix)]
            async fn assert_not_regular(path: &Path) {
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    run(&json!({ "path": path.to_str().expect("utf8 path") })),
                )
                .await
                .expect("read_image must reject before a special file can block")
                .unwrap_err();
                assert!(
                    matches!(result, ToolError::NotRegularFile { .. }),
                    "got {result:?}"
                );
            }

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

            #[cfg(unix)]
            #[tokio::test]
            async fn reads_symlink_to_regular_image() {
                let mut file = NamedTempFile::new().expect("tempfile");
                file.write_all(b"\x89PNG\r\n\x1a\nabc").expect("write");
                let dir = tempfile::tempdir().expect("tempdir");
                let link = dir.path().join("image-link");
                std::os::unix::fs::symlink(file.path(), &link).expect("symlink");

                let out = run(&json!({ "path": link.to_str().expect("utf8 path") }))
                    .await
                    .expect("symlink to regular image accepted");
                assert_eq!(
                    out.get("media_type").and_then(Value::as_str),
                    Some("image/png")
                );
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn rejects_device_alias_and_socket() {
                assert_not_regular(Path::new("/dev/null")).await;

                let dir = tempfile::tempdir().expect("tempdir");
                let link = dir.path().join("device-link");
                std::os::unix::fs::symlink("/dev/null", &link).expect("symlink");
                assert_not_regular(&link).await;

                let socket = dir.path().join("socket");
                let _listener = UnixListener::bind(&socket).expect("bind unix socket");
                assert_not_regular(&socket).await;
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
    }

    pub mod shell_script {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/shell_script.rs"
        ));
    }

    pub mod write_file {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/write_file.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;
            use tempfile::tempdir;

            #[tokio::test]
            async fn writes_text_and_reports_byte_count() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("out.txt");
                let path_str = path.to_str().unwrap();
                let out = run(&json!({"path": path_str, "new_string": "hello"}))
                    .await
                    .unwrap();
                assert!(out.starts_with("wrote 5 bytes to"));
                let read_back = std::fs::read_to_string(&path).unwrap();
                assert_eq!(read_back, "hello");
            }

            #[tokio::test]
            async fn empty_content_truncates_to_zero() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("empty.txt");
                std::fs::write(&path, "preexisting").unwrap();
                let path_str = path.to_str().unwrap();
                run(&json!({"path": path_str, "new_string": ""}))
                    .await
                    .unwrap();
                let read_back = std::fs::read_to_string(&path).unwrap();
                assert!(read_back.is_empty());
            }

            #[tokio::test]
            async fn creates_missing_parent_directories() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("a/b/c/file.txt");
                let path_str = path.to_str().unwrap();
                run(&json!({"path": path_str, "new_string": "hi"}))
                    .await
                    .unwrap();
                let read_back = std::fs::read_to_string(&path).unwrap();
                assert_eq!(read_back, "hi");
            }

            #[tokio::test]
            async fn rejects_existing_directory_path() {
                let dir = tempdir().unwrap();
                let path_str = dir.path().to_str().unwrap();
                let err = run(&json!({"path": path_str, "new_string": "x"}))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::IsDirectory { .. }));
            }

            #[tokio::test]
            async fn rejects_missing_path_field() {
                let err = run(&json!({"new_string": "x"})).await.unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }));
            }

            #[tokio::test]
            async fn rejects_missing_new_string_field() {
                let err = run(&json!({"path": "/tmp/out"})).await.unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }));
            }

            #[tokio::test]
            async fn rejects_empty_path() {
                let err = run(&json!({"path": "", "new_string": "x"}))
                    .await
                    .unwrap_err();
                assert!(matches!(err, ToolError::BadArgs { .. }));
            }

            #[test]
            fn schema_requires_path_and_new_string() {
                let s = schema();
                let req = s.get("required").and_then(Value::as_array).unwrap();
                let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
                assert!(names.contains(&"path"));
                assert!(names.contains(&"new_string"));
            }

            #[tokio::test]
            async fn replaces_one_exact_match() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("file.txt");
                std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
                let out = run(&json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "beta",
                    "new_string": "BETA"
                }))
                .await
                .unwrap();
                assert!(out.contains("edited"));
                assert_eq!(
                    std::fs::read_to_string(path).unwrap(),
                    "alpha\nBETA\ngamma\n"
                );
            }

            #[tokio::test]
            async fn exact_edit_allows_empty_replacement() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("file.txt");
                std::fs::write(&path, "keep remove keep").unwrap();
                run(&json!({
                    "path": path.to_str().unwrap(),
                    "old_string": " remove",
                    "new_string": ""
                }))
                .await
                .unwrap();
                assert_eq!(std::fs::read_to_string(path).unwrap(), "keep keep");
            }

            #[tokio::test]
            async fn exact_edit_rejects_missing_or_ambiguous_match() {
                let dir = tempdir().unwrap();
                let path = dir.path().join("file.txt");
                std::fs::write(&path, "x x").unwrap();
                for old_string in ["missing", "x"] {
                    let err = run(&json!({
                        "path": path.to_str().unwrap(),
                        "old_string": old_string,
                        "new_string": "y"
                    }))
                    .await
                    .unwrap_err();
                    assert!(matches!(err, ToolError::BadArgs { .. }));
                }
            }
        }
    }

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/runtime.rs"));

    #[cfg(test)]
    mod tests {
        use std::collections::HashSet;

        use super::*;

        #[test]
        fn actual_advertised_inventory_has_complete_display_coverage() {
            let expected = [
                read_file::NAME,
                read_image::NAME,
                write_file::NAME,
                process_exec::NAME,
                shell_script::NAME,
            ]
            .into_iter()
            .collect::<HashSet<_>>();
            let actual = TOOLS.iter().map(|tool| tool.name).collect::<HashSet<_>>();
            assert_eq!(
                actual, expected,
                "update semantic display coverage when the active registry changes"
            );
            for descriptor in TOOLS {
                let display = (descriptor.display)();
                assert!(display.get("compact").is_some(), "{}", descriptor.name);
                assert!(display.get("expanded").is_some(), "{}", descriptor.name);
                assert!(display.get("result").is_some(), "{}", descriptor.name);
            }
        }

        #[test]
        fn module_owned_display_functions_compile() {
            let displays: [fn() -> Value; 5] = [
                read_file::display,
                read_image::display,
                write_file::display,
                process_exec::display,
                shell_script::display,
            ];
            assert_eq!(displays.len(), TOOLS.len());
            assert!(displays.into_iter().all(|display| display().is_object()));
        }
    }
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_body_advertises_plugin_version() {
        let b = hello_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.hello")
        );
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
    }

    #[test]
    fn tool_register_body_lists_every_tool_with_schema() {
        let b = tool_register_body();
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("tool.register"));
        let arr = b.get("tools").and_then(Value::as_array).expect("tools");
        assert_eq!(arr.len(), TOOLS.len());
        let read_file = arr
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some("read_file"))
            .expect("read_file in tools");
        assert!(read_file
            .get("description")
            .and_then(Value::as_str)
            .is_some());
        let params = read_file.get("parameters").expect("parameters");
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(
            read_file.get("context").is_none(),
            "public tool.register must not expose internal context metadata"
        );
    }

    #[test]
    fn tools_advertise_body_carries_private_context_metadata() {
        let b = tools_advertise_body("tool-gate");
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("tool-gate.tools.advertise")
        );
        let arr = b.get("tools").and_then(Value::as_array).expect("tools");
        let read_file = arr
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some("read_file"))
            .expect("read_file in tools");
        let folders = read_file
            .get("context")
            .and_then(|v| v.get("folders"))
            .and_then(Value::as_array)
            .expect("context.folders");
        assert_eq!(folders.len(), 1);
    }

    #[test]
    fn tool_result_ok_body_carries_id_and_output() {
        let b = tool_result_ok_body("call-1", Value::String("hello".into()));
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call-1"));
        assert_eq!(b.get("output").and_then(Value::as_str), Some("hello"));
        assert!(!b.contains_key("error"));
    }

    #[test]
    fn tool_result_error_body_carries_id_and_error() {
        let b = tool_result_error_body("call-2", "boom");
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call-2"));
        assert_eq!(b.get("error").and_then(Value::as_str), Some("boom"));
        assert!(!b.contains_key("output"));
    }

    #[test]
    fn goodbye_body_uses_plugin_prefix() {
        let b = goodbye_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.goodbye")
        );
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    // End-to-end dispatch: feed a `basic-tools.tool.invoke` event into
    // `dispatch_event` and verify a matching `tool.result { output }`
    // emerges on the writer channel.
    #[tokio::test]
    async fn dispatch_invoke_read_file_emits_result() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        crate::tools::read_file::configure_max_bytes(1024 * 1024).ok();
        let mut f = NamedTempFile::new().expect("tempfile");
        write!(f, "abc").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();

        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-7",
            "name": "read_file",
            "args": { "path": path }
        })
        .as_object()
        .expect("obj")
        .clone();

        dispatch_event(&tx, &body).await.expect("dispatch ok");

        let msg = rx.recv().await.expect("got reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-7"));
        assert_eq!(body.get("output").and_then(Value::as_str), Some("abc"));
    }

    // Invoke with a non-existent path produces a `tool.result { error }`.
    #[tokio::test]
    async fn dispatch_invoke_missing_file_emits_error() {
        crate::tools::read_file::configure_max_bytes(1024 * 1024).ok();
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-8",
            "name": "read_file",
            "args": { "path": "/definitely/does/not/exist/qzx" }
        })
        .as_object()
        .expect("obj")
        .clone();

        dispatch_event(&tx, &body).await.expect("dispatch ok");

        let msg = rx.recv().await.expect("got reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-8"));
        let err = body.get("error").and_then(Value::as_str).expect("error");
        assert!(err.contains("file not found"), "got: {err}");
        assert!(body.get("output").is_none());
    }

    // Missing `id` is dropped silently (no caller to address) — verify no
    // reply is emitted.
    #[tokio::test]
    async fn dispatch_invoke_missing_id_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "name": "read_file",
            "args": { "path": "/tmp/whatever" }
        })
        .as_object()
        .expect("obj")
        .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        // Drop the sender so try_recv returns Disconnected (or Empty if
        // we got here too fast).
        drop(tx);
        match rx.recv().await {
            None => {}
            Some(unexpected) => panic!("expected no reply, got {}", unexpected.to_line()),
        }
    }

    // Missing `name` — but with a valid `id` — produces a tool.result with
    // an error so the caller can correlate.
    #[tokio::test]
    async fn dispatch_invoke_missing_name_emits_error() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-9"
        })
        .as_object()
        .expect("obj")
        .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        let msg = rx.recv().await.expect("reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-9"));
        assert!(body.get("error").is_some());
    }

    // The plugin event loop must not let one long-running tool invocation
    // block later independent invocations. Graphs rely on this for async
    // branch execution: a fast node should not wait behind a slow sibling.
    #[tokio::test]
    async fn dispatch_ignores_unrelated_events() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({ "kind": "ollama.stream.delta", "text": "hi" })
            .as_object()
            .expect("obj")
            .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        drop(tx);
        assert!(rx.recv().await.is_none());
    }
}
