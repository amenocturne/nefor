//! `search_text` — recursive regex search across files using ripgrep's engine.
//!
//! Embeds `grep-regex` + `grep-searcher` + `ignore` as library crates rather
//! than shelling out to `rg`. Respects `.gitignore` by default.

use std::io;
use std::path::Path;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::error::ToolError;

pub const NAME: &str = "search_text";
pub const DESCRIPTION: &str = "Search for a regex pattern in files under a path (recursively). \
     Returns matching lines as `path:line:match`. Respects .gitignore. \
     Supports file-type filters, globs, case-insensitive and literal matching.";

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_MAX_RESULTS: usize = 2000;
const MAX_RESULT_LINE_BYTES: usize = 4 * 1024;
const MAX_TOTAL_OUTPUT_BYTES: usize = 24 * 1024;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regex pattern. Use `fixed_string: true` for literal matching."
            },
            "path": {
                "type": "string",
                "description": "Search root (file or directory). Defaults to '.'."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory for resolving relative paths."
            },
            "max_results": {
                "type": "integer",
                "description": "Cap on returned match lines (default 200, max 2000)."
            },
            "file_type": {
                "type": "string",
                "description": "Filter by file type, e.g. 'py', 'rs', 'lua', 'md'. Same as rg -t."
            },
            "glob": {
                "type": "string",
                "description": "Include glob pattern, e.g. '*.md', 'src/**/*.rs'."
            },
            "exclude_glob": {
                "type": "string",
                "description": "Exclude glob pattern, e.g. 'node_modules', '*.min.js'."
            },
            "case_insensitive": {
                "type": "boolean",
                "description": "Case-insensitive matching (default false)."
            },
            "fixed_string": {
                "type": "boolean",
                "description": "Treat pattern as a literal string, not a regex (default false)."
            },
            "files_only": {
                "type": "boolean",
                "description": "List file paths containing matches instead of match lines (default false)."
            },
            "context_lines": {
                "type": "integer",
                "description": "Lines of context around each match (like rg -C)."
            },
            "max_filesize": {
                "type": "integer",
                "description": "Skip files larger than this many bytes (default: no limit beyond ignore defaults)."
            }
        },
        "required": ["pattern"]
    })
}

/// Declarative presentation metadata advertised with this tool.
pub fn display() -> Value {
    json!({
        "label": "Search text",
        "primary": { "arg": "pattern" },
        "arguments": [
            { "label": "in", "arg": "path", "cwd_arg": "cwd", "default": "." },
            { "label": "type", "arg": "file_type" },
            { "label": "glob", "arg": "glob" },
            { "label": "limit", "arg": "max_results" }
        ],
        "result": { "kind": "content" }
    })
}

pub async fn run(args: &Value) -> Result<String, ToolError> {
    let parsed = parse_args(args)?;
    tokio::task::spawn_blocking(move || search(parsed))
        .await
        .map_err(|e| ToolError::Io {
            path: "(search)".into(),
            message: format!("search task panicked: {e}"),
        })?
}

struct ParsedArgs {
    pattern: String,
    path: String,
    cwd: Option<String>,
    max_results: usize,
    file_type: Option<String>,
    glob: Option<String>,
    exclude_glob: Option<String>,
    case_insensitive: bool,
    fixed_string: bool,
    files_only: bool,
    context_lines: Option<usize>,
    max_filesize: Option<u64>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;

    let pattern = obj
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `pattern`".into(),
        })?;
    if pattern.is_empty() {
        return Err(ToolError::BadArgs {
            tool: NAME.into(),
            message: "`pattern` must be non-empty".into(),
        });
    }

    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");

    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let max_results = obj
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_MAX_RESULTS))
        .unwrap_or(DEFAULT_MAX_RESULTS);

    let file_type = obj
        .get("file_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let glob = obj
        .get("glob")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let exclude_glob = obj
        .get("exclude_glob")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let case_insensitive = obj
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let fixed_string = obj
        .get("fixed_string")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let files_only = obj
        .get("files_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let context_lines = obj
        .get("context_lines")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    let max_filesize = obj.get("max_filesize").and_then(Value::as_u64);

    Ok(ParsedArgs {
        pattern: pattern.to_owned(),
        path: path.to_owned(),
        cwd,
        max_results,
        file_type,
        glob,
        exclude_glob,
        case_insensitive,
        fixed_string,
        files_only,
        context_lines,
        max_filesize,
    })
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_owned();
    }
    match cwd {
        Some(dir) => Path::new(dir).join(p).to_string_lossy().into_owned(),
        None => path.to_owned(),
    }
}

fn search(args: ParsedArgs) -> Result<String, ToolError> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(args.case_insensitive)
        .fixed_strings(args.fixed_string)
        .build(&args.pattern)
        .map_err(|e| ToolError::BadArgs {
            tool: NAME.into(),
            message: format!("invalid pattern: {e}"),
        })?;

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder.line_number(true);
    if let Some(ctx) = args.context_lines {
        searcher_builder.before_context(ctx);
        searcher_builder.after_context(ctx);
    }
    let mut searcher = searcher_builder.build();

    let search_path = resolve_path(&args.path, args.cwd.as_deref());
    let root = Path::new(&search_path);

    let mut results = SearchResults::new(args.max_results);

    if root.is_file() {
        let path_str = root.to_string_lossy();
        let mut sink = MatchSink::new(&path_str, &matcher, &mut results, args.files_only);
        searcher
            .search_path(&matcher, root, &mut sink)
            .map_err(|e| ToolError::Io {
                path: search_path.clone(),
                message: e.to_string(),
            })?;
    } else {
        let mut walk_builder = WalkBuilder::new(root);
        walk_builder.hidden(true); // skip hidden files (rg default)

        if let Some(ref ft) = args.file_type {
            let mut types_builder = TypesBuilder::new();
            types_builder.add_defaults();
            types_builder.select(ft);
            let types = types_builder.build().map_err(|e| ToolError::BadArgs {
                tool: NAME.into(),
                message: format!("invalid file type `{ft}`: {e}"),
            })?;
            walk_builder.types(types);
        }

        let has_overrides = args.glob.is_some() || args.exclude_glob.is_some();
        if has_overrides {
            let mut ob = OverrideBuilder::new(root);
            if let Some(ref g) = args.glob {
                ob.add(g).map_err(|e| ToolError::BadArgs {
                    tool: NAME.into(),
                    message: format!("invalid glob `{g}`: {e}"),
                })?;
            }
            if let Some(ref eg) = args.exclude_glob {
                ob.add(&format!("!{eg}")).map_err(|e| ToolError::BadArgs {
                    tool: NAME.into(),
                    message: format!("invalid exclude glob `{eg}`: {e}"),
                })?;
            }
            let overrides = ob.build().map_err(|e| ToolError::BadArgs {
                tool: NAME.into(),
                message: format!("glob build error: {e}"),
            })?;
            walk_builder.overrides(overrides);
        }

        if let Some(max_size) = args.max_filesize {
            walk_builder.max_filesize(Some(max_size));
        }

        walk_builder.sort_by_file_path(|a, b| a.cmp(b));

        for entry in walk_builder.build() {
            if results.at_limit() {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    return Err(ToolError::Io {
                        path: search_path.clone(),
                        message: e.to_string(),
                    })
                }
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path_str = entry.path().to_string_lossy();
            let mut sink = MatchSink::new(&path_str, &matcher, &mut results, args.files_only);
            searcher
                .search_path(&matcher, entry.path(), &mut sink)
                .map_err(|e| ToolError::Io {
                    path: entry.path().to_string_lossy().into_owned(),
                    message: e.to_string(),
                })?;
        }
    }

    if results.lines.is_empty() {
        return Ok("(no matches)".into());
    }

    Ok(results.finish())
}

struct SearchResults {
    lines: Vec<String>,
    bytes: usize,
    max_results: usize,
    truncated: bool,
}

impl SearchResults {
    fn new(max_results: usize) -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            max_results,
            truncated: false,
        }
    }

    fn at_limit(&self) -> bool {
        self.lines.len() >= self.max_results || self.truncated
    }

    fn push(&mut self, line: String) -> bool {
        if self.lines.len() >= self.max_results {
            self.truncated = true;
            return false;
        }
        let separator = usize::from(!self.lines.is_empty());
        if self.bytes + separator + line.len() > MAX_TOTAL_OUTPUT_BYTES {
            self.truncated = true;
            return false;
        }
        self.bytes += separator + line.len();
        self.lines.push(line);
        true
    }

    fn finish(mut self) -> String {
        if self.lines.len() >= self.max_results {
            self.truncated = true;
        }
        let mut output = self.lines.join("\n");
        if self.truncated {
            output.push_str("\n[...truncated: search_text output reached result/24 KiB limit]");
        }
        output
    }
}

struct MatchSink<'a> {
    path: &'a str,
    matcher: &'a RegexMatcher,
    results: &'a mut SearchResults,
    files_only: bool,
    file_recorded: bool,
}

impl<'a> MatchSink<'a> {
    fn new(
        path: &'a str,
        matcher: &'a RegexMatcher,
        results: &'a mut SearchResults,
        files_only: bool,
    ) -> Self {
        Self {
            path,
            matcher,
            results,
            files_only,
            file_recorded: false,
        }
    }

    fn at_limit(&self) -> bool {
        self.results.at_limit()
    }
}

impl Sink for MatchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.at_limit() {
            return Ok(false);
        }
        if self.files_only {
            if !self.file_recorded {
                self.file_recorded = true;
                self.results.push(self.path.to_owned());
            }
            return Ok(false);
        }
        let line_num = mat.line_number().unwrap_or(0);
        let bytes = trim_line_end(mat.bytes());
        if bytes.len() <= MAX_RESULT_LINE_BYTES {
            self.results.push(format!(
                "{}:{}:{}",
                self.path,
                line_num,
                String::from_utf8_lossy(bytes)
            ));
        } else {
            let absolute_line_offset = mat.absolute_byte_offset();
            let path = self.path;
            let results = &mut self.results;
            self.matcher
                .find_iter(bytes, |found| {
                    let preview = bounded_match_preview(bytes, found.start(), found.end());
                    results.push(format!(
                        "{}:{}:byte {}..{}:{}",
                        path,
                        line_num,
                        absolute_line_offset + found.start() as u64,
                        absolute_line_offset + found.end() as u64,
                        preview
                    ))
                })
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        Ok(!self.at_limit())
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.at_limit() || self.files_only {
            return Ok(false);
        }
        let line_num = ctx.line_number().unwrap_or(0);
        let content = bounded_line_preview(ctx.bytes());
        let sep = match ctx.kind() {
            &SinkContextKind::Before | &SinkContextKind::After => "-",
            _ => "-",
        };
        self.results
            .push(format!("{}:{}{}{}", self.path, line_num, sep, content));
        Ok(!self.at_limit())
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        if self.at_limit() || self.files_only {
            return Ok(false);
        }
        self.results.push("--".into());
        Ok(true)
    }
}

fn bounded_line_preview(bytes: &[u8]) -> String {
    let bytes = trim_line_end(bytes);
    if bytes.len() <= MAX_RESULT_LINE_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut end = MAX_RESULT_LINE_BYTES;
    while end > 0 && !is_utf8_boundary_byte(bytes[end]) {
        end -= 1;
    }
    if end == 0 {
        end = MAX_RESULT_LINE_BYTES;
    }

    let preview = String::from_utf8_lossy(&bytes[..end]);
    format!(
        "{}[... search_text line truncated: {} bytes omitted from this matching line]",
        preview,
        bytes.len().saturating_sub(end)
    )
}

fn bounded_match_preview(bytes: &[u8], match_start: usize, match_end: usize) -> String {
    let match_len = match_end.saturating_sub(match_start);
    let kept_match_len = match_len.min(MAX_RESULT_LINE_BYTES);
    let surrounding = MAX_RESULT_LINE_BYTES - kept_match_len;
    let mut start = match_start.saturating_sub(surrounding / 2);
    let mut end = (start + MAX_RESULT_LINE_BYTES).min(bytes.len());
    start = end.saturating_sub(MAX_RESULT_LINE_BYTES);

    while start < match_start && !is_utf8_boundary_byte(bytes[start]) {
        start += 1;
    }
    while end > start && end < bytes.len() && !is_utf8_boundary_byte(bytes[end]) {
        end -= 1;
    }

    let prefix = if start > 0 {
        format!("[... {} bytes omitted before match]", start)
    } else {
        String::new()
    };
    let suffix = if end < bytes.len() {
        format!("[... {} bytes omitted after match]", bytes.len() - end)
    } else {
        String::new()
    };
    format!(
        "{}{}{}",
        prefix,
        String::from_utf8_lossy(&bytes[start..end]),
        suffix
    )
}

fn trim_line_end(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn is_utf8_boundary_byte(byte: u8) -> bool {
    (byte & 0b1100_0000) != 0b1000_0000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_test_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("hello.txt"),
            "Hello World\nhello rust\nGoodbye\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(dir.path().join("data.py"), "x = 1\ny = 2\nhello = 3\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn basic_search() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({"pattern": "hello", "path": path}))
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(!out.contains("(no matches)"));
    }

    #[tokio::test]
    async fn case_insensitive_search() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": path,
            "case_insensitive": true
        }))
        .await
        .unwrap();
        assert!(out.contains("Hello World"));
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn fixed_string_search() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        // "." in regex matches any char; fixed_string treats it literally
        let out = run(&json!({
            "pattern": "println!",
            "path": path,
            "fixed_string": true
        }))
        .await
        .unwrap();
        assert!(out.contains("println!"));
    }

    #[tokio::test]
    async fn file_type_filter() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": path,
            "file_type": "rust"
        }))
        .await
        .unwrap();
        assert!(out.contains("code.rs"));
        assert!(!out.contains("hello.txt"));
        assert!(!out.contains("data.py"));
    }

    #[tokio::test]
    async fn glob_filter() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": path,
            "glob": "*.txt"
        }))
        .await
        .unwrap();
        assert!(out.contains("hello.txt"));
        assert!(!out.contains("code.rs"));
    }

    #[tokio::test]
    async fn exclude_glob() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": path,
            "exclude_glob": "*.py"
        }))
        .await
        .unwrap();
        assert!(!out.contains("data.py"));
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn files_only_mode() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": path,
            "files_only": true
        }))
        .await
        .unwrap();
        // Should contain file paths, not line contents
        let lines: Vec<&str> = out.lines().collect();
        for line in &lines {
            assert!(
                !line.contains(':'),
                "files_only should not have :line:content — got: {line}"
            );
        }
    }

    #[tokio::test]
    async fn max_results_caps_output() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": ".",
            "path": path,
            "max_results": 2
        }))
        .await
        .unwrap();
        let match_lines: Vec<&str> = out.lines().filter(|l| !l.starts_with("[...")).collect();
        assert!(match_lines.len() <= 2);
        assert!(out.contains("[...truncated"));
    }

    #[tokio::test]
    async fn huge_matching_line_is_previewed() {
        let dir = TempDir::new().unwrap();
        let huge_line = format!("prefix-needle-{}\n", "x".repeat(MAX_RESULT_LINE_BYTES * 4));
        fs::write(dir.path().join("session.jsonl"), huge_line).unwrap();

        let out = run(&json!({
            "pattern": "needle",
            "path": dir.path().to_str().unwrap()
        }))
        .await
        .unwrap();

        assert!(
            out.len() < MAX_RESULT_LINE_BYTES * 2,
            "search_text should not return the whole huge matching line; got {} bytes",
            out.len()
        );
        assert!(out.contains("prefix-needle-"));
        assert!(out.contains("bytes omitted"));
        assert!(out.contains("byte "));
    }

    #[tokio::test]
    async fn huge_one_line_json_reports_each_match_with_bounded_excerpts() {
        let dir = TempDir::new().unwrap();
        let huge_line = format!(
            "{{\"head\":\"{}\",\"first\":\"needle-one\",\"middle\":\"{}\",\"second\":\"needle-two\",\"tail\":\"{}\"}}",
            "a".repeat(1024 * 1024),
            "b".repeat(1024 * 1024),
            "c".repeat(1024 * 1024)
        );
        fs::write(dir.path().join("huge.json"), huge_line).unwrap();

        let out = run(&json!({
            "pattern": "needle-(one|two)",
            "path": dir.path().to_str().unwrap()
        }))
        .await
        .unwrap();

        assert!(out.contains("needle-one"));
        assert!(out.contains("needle-two"));
        assert_eq!(out.matches(":byte ").count(), 2);
        assert!(out.len() < 12 * 1024);
    }

    #[tokio::test]
    async fn huge_line_preview_preserves_unicode_at_excerpt_boundaries() {
        let dir = TempDir::new().unwrap();
        let huge_line = format!("{}needle{}", "界".repeat(2000), "€".repeat(2000));
        fs::write(dir.path().join("unicode.json"), huge_line).unwrap();

        let out = run(&json!({
            "pattern": "needle",
            "path": dir.path().to_str().unwrap()
        }))
        .await
        .unwrap();

        assert!(out.contains("界"));
        assert!(out.contains("€"));
        assert!(!out.contains('\u{fffd}'));
    }

    #[tokio::test]
    async fn total_output_is_bounded_below_spill_threshold() {
        let dir = TempDir::new().unwrap();
        let mut huge_line = String::new();
        for i in 0..100 {
            huge_line.push_str(&format!(
                "{}needle-{i}{}",
                "x".repeat(5000),
                "y".repeat(5000)
            ));
        }
        fs::write(dir.path().join("spill.txt"), huge_line).unwrap();

        let out = run(&json!({
            "pattern": "needle-[0-9]+",
            "path": dir.path().to_str().unwrap(),
            "max_results": 200
        }))
        .await
        .unwrap();

        assert!(
            out.len() < 32 * 1024,
            "bounded search output must not spill again"
        );
        assert!(out.contains("truncated"));
    }

    #[tokio::test]
    async fn ordinary_small_lines_keep_the_existing_shape() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("normal.txt");
        fs::write(&path, "alpha needle omega\n").unwrap();

        let out = run(&json!({ "pattern": "needle", "path": path }))
            .await
            .unwrap();
        assert_eq!(out, format!("{}:1:alpha needle omega", path.display()));
        assert!(!out.contains(":byte "));
    }

    #[tokio::test]
    async fn no_matches_returns_marker() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({"pattern": "zzzznotfound", "path": path}))
            .await
            .unwrap();
        assert_eq!(out, "(no matches)");
    }

    #[tokio::test]
    async fn context_lines() {
        let dir = make_test_dir();
        let path = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello rust",
            "path": path,
            "context_lines": 1
        }))
        .await
        .unwrap();
        // Should include surrounding lines from hello.txt
        assert!(out.contains("Hello World") || out.contains("Goodbye"));
    }

    #[tokio::test]
    async fn single_file_search() {
        let dir = make_test_dir();
        let file_path = dir.path().join("hello.txt");
        let path = file_path.to_str().unwrap();
        let out = run(&json!({"pattern": "hello", "path": path}))
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(!out.contains("code.rs"));
    }

    #[tokio::test]
    async fn rejects_empty_pattern() {
        let err = run(&json!({"pattern": ""})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_missing_pattern() {
        let err = run(&json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_invalid_regex() {
        let err = run(&json!({"pattern": "[invalid"})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn cwd_resolves_relative_path() {
        let dir = make_test_dir();
        let cwd = dir.path().to_str().unwrap();
        let out = run(&json!({
            "pattern": "hello",
            "path": ".",
            "cwd": cwd
        }))
        .await
        .unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn schema_requires_pattern_only() {
        let s = schema();
        let req = s.get("required").and_then(Value::as_array).unwrap();
        let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["pattern"]);
    }
}
