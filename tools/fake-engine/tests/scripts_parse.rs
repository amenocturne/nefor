//! Smoke test: every shipped example script parses without error.
//!
//! The harness can't use the public script parser across crate boundaries
//! without re-exporting, so this test lives next to the binary and
//! re-parses via a duplicated tiny shim. To avoid duplication we just use
//! the envelope/outgoing parsers directly, which is what the script
//! parser ultimately delegates to.

use std::path::Path;

use nefor_protocol::{Envelope, PluginOutgoing};

fn assert_script_lines_parse(path: &Path) {
    let src =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    for (idx, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let looks_stamped = trimmed.contains("\"from\"") || trimmed.contains("\"ts\"");
        let result = if looks_stamped {
            Envelope::parse_line(trimmed).map(|_| ())
        } else {
            PluginOutgoing::parse_line(trimmed).map(|_| ())
        };
        result.unwrap_or_else(|e| {
            panic!(
                "{}:{}: failed to parse line: {e}\n  line: {trimmed}",
                path.display(),
                idx + 1
            )
        });
    }
}

#[test]
fn hello_world_script_parses() {
    assert_script_lines_parse(Path::new("scripts/hello-world.jsonl"));
}

#[test]
fn echo_keys_script_parses() {
    assert_script_lines_parse(Path::new("scripts/echo-keys.jsonl"));
}
