use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(120);

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn source_files(root: &Path, extension: &str) -> Vec<PathBuf> {
    fn visit(dir: &Path, extension: &str, paths: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("read corpus directory {}: {error}", dir.display()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("read entry under {}: {error}", dir.display()));
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|name| name.to_str());
                if !matches!(name, Some(".git" | ".worktrees" | "target" | "tmp")) {
                    visit(&path, extension, paths);
                }
            } else if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
                paths.push(path);
            }
        }
    }

    let mut paths = Vec::new();
    visit(root, extension, &mut paths);
    paths.sort();
    paths
}

fn mag_files(root: &Path) -> Vec<PathBuf> {
    source_files(root, "mag")
}

fn module_name(lib_root: &Path, path: &Path) -> String {
    path.strip_prefix(lib_root)
        .unwrap_or_else(|_| panic!("{} is not under {}", path.display(), lib_root.display()))
        .with_extension("")
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join(".")
}

async fn spawn_mag(data_dir: &Path) -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel")
        .arg(kernel_path())
        .env("NEFOR_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn mag-plugin")
}

async fn read_outgoing<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    expecting: &str,
) -> PluginOutgoing {
    let mut line = String::new();
    match timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => panic!("mag stdout closed while expecting {expecting}"),
        Ok(Ok(_)) => PluginOutgoing::parse_line(line.trim_end()).expect("parse mag output"),
        Ok(Err(error)) => panic!("read mag stdout while expecting {expecting}: {error}"),
        Err(_) => panic!("timed out waiting for mag output while expecting {expecting}"),
    }
}

async fn write_envelope(stdin: &mut ChildStdin, envelope: Envelope) {
    stdin
        .write_all(envelope.to_line().as_bytes())
        .await
        .expect("write envelope");
    stdin.write_all(b"\n").await.expect("write newline");
    stdin.flush().await.expect("flush envelope");
}

async fn send_event(stdin: &mut ChildStdin, body: Map<String, Value>) {
    write_envelope(
        stdin,
        Envelope::event(PluginName::engine(), Timestamp::now(), body),
    )
    .await;
}

async fn handshake<R: AsyncBufReadExt + Unpin>(reader: &mut R, stdin: &mut ChildStdin) {
    let ready = read_outgoing(reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
    write_envelope(
        stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;
}

async fn load<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    id: &str,
    source_dir: &Path,
    entry: &Path,
    module_roots: &[PathBuf],
) -> Map<String, Value> {
    send_event(
        stdin,
        json!({
            "kind": "mag.load",
            "id": id,
            "source_dir": source_dir.to_string_lossy(),
            "entry": entry.to_string_lossy(),
            "module_roots": module_roots,
        })
        .as_object()
        .expect("load body is an object")
        .clone(),
    )
    .await;

    loop {
        let outgoing = read_outgoing(reader, id).await;
        if let Body::Event(body) = outgoing.body {
            if body.get("in_reply_to").and_then(Value::as_str) == Some(id) {
                return body;
            }
        }
    }
}

async fn shutdown(mut stdin: ChildStdin, mut child: Child) {
    write_envelope(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::Shutdown {
                reason: None,
                grace_ms: None,
            },
        ),
    )
    .await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(10), child.wait()).await;
}

#[test]
fn active_starter_prompts_and_mock_do_not_use_removed_mag_teaching_forms() {
    let root = repo_root();
    let mut paths = source_files(&root.join("starter/prompts"), "md");
    paths.extend(source_files(&root.join("starter/mag/lib/prompts"), "md"));
    paths.push(root.join("starter/mag/lib/patterns.md"));
    let guidance_paths = paths.clone();
    paths.push(root.join("starter/mock-provider/init.lua"));
    let removed = [
        "(agent ",
        "(node ",
        "(graph ",
        "(subgraph ",
        "(bash ",
        " :terminal ",
        "`->` pipes",
        "-> pipes",
        "-> connects",
        "-> composes",
    ];

    for path in &paths {
        let source = fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "read active MAG teaching source {}: {error}",
                path.display()
            )
        });
        for obsolete in removed {
            assert!(
                !source.contains(obsolete),
                "active MAG teaching source {} contains obsolete form {obsolete:?}",
                path.display()
            );
        }
    }

    for path in guidance_paths {
        let source = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "read active MAG teaching source {}: {error}",
                path.display()
            )
        });
        if source.contains("mag-eval") {
            assert!(
                source.contains("intent"),
                "active MAG teaching source {} omits intent guidance",
                path.display()
            );
            assert!(
                source.contains("1–5 word") || source.contains("1–5-word"),
                "active MAG teaching source {} omits the 1–5-word intent constraint",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn shipped_mag_corpus_compiles_with_runtime_contracts() {
    let root = repo_root();
    let starter = root.join("starter");
    let lib_root = starter.join("mag/lib");
    let fixture_root = starter.join("mag/tests");
    let all_mag = mag_files(&root);
    let libraries = mag_files(&lib_root);
    let fixtures = mag_files(&fixture_root);
    let entrypoints = all_mag
        .iter()
        .filter(|path| {
            path.starts_with(&starter)
                && !path.starts_with(&lib_root)
                && !path.starts_with(&fixture_root)
        })
        .cloned()
        .collect::<Vec<_>>();

    let unclassified = all_mag
        .iter()
        .filter(|path| {
            !libraries.contains(path) && !fixtures.contains(path) && !entrypoints.contains(path)
        })
        .collect::<Vec<_>>();
    assert!(
        unclassified.is_empty(),
        "shipped MAG files are outside the known library, entrypoint, or failure-fixture categories: {unclassified:#?}"
    );

    assert!(
        !libraries.is_empty(),
        "no shipped MAG library modules found"
    );
    assert!(!entrypoints.is_empty(), "no shipped MAG entrypoints found");

    let missing_expectations = fixtures
        .iter()
        .filter(|path| !path.with_extension("error").is_file())
        .collect::<Vec<_>>();
    assert!(
        missing_expectations.is_empty(),
        "MAG failure fixtures missing matching .error files: {missing_expectations:#?}"
    );

    let orphan_expectations = source_files(&fixture_root, "error")
        .into_iter()
        .filter(|path| !path.with_extension("mag").is_file())
        .collect::<Vec<_>>();
    assert!(
        orphan_expectations.is_empty(),
        "MAG .error files missing matching fixtures: {orphan_expectations:#?}"
    );

    let temp_root = std::env::temp_dir().join(format!("mag-corpus-{}", std::process::id()));
    fs::create_dir_all(&temp_root).expect("create MAG corpus temp directory");
    let synthetic = libraries
        .iter()
        .map(|path| format!("(require \"{}\")", module_name(&lib_root, path)))
        .chain(std::iter::once(
            "(artifact \"test.mag-corpus/v1\" {})".to_owned(),
        ))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(temp_root.join("all-libraries.mag"), synthetic)
        .expect("write synthetic MAG library entrypoint");

    let mut child = spawn_mag(&temp_root).await;
    let mut stdin = child.stdin.take().expect("mag stdin");
    let stdout = child.stdout.take().expect("mag stdout");
    let mut reader = BufReader::new(stdout);
    let stderr = child.stderr.take().expect("mag stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });
    handshake(&mut reader, &mut stdin).await;

    let library_result = load(
        &mut reader,
        &mut stdin,
        "corpus-libraries",
        &temp_root,
        Path::new("all-libraries.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        library_result.get("kind").and_then(Value::as_str),
        Some("mag.loaded"),
        "shipped MAG libraries failed to compile: {library_result:#?}"
    );

    // The first Lisp fence in patterns.md is the canonical minimal agent
    // program injected into every lead turn. Compile that exact text rather
    // than maintaining a test-side approximation that can drift from the
    // documentation agents actually see.
    let patterns_path = lib_root.join("patterns.md");
    let patterns = fs::read_to_string(&patterns_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", patterns_path.display()));
    let canonical = patterns
        .split_once("```lisp\n")
        .and_then(|(_, rest)| rest.split_once("\n```").map(|(source, _)| source))
        .expect("patterns.md contains a complete canonical Lisp fence");
    assert!(
        canonical.contains("(nefor.actors.agent"),
        "the first Lisp fence must remain the canonical minimal agent program"
    );
    assert!(
        !canonical.contains("\"mag\""),
        "ordinary child agents in the canonical example must not receive the lead orchestration tool"
    );
    assert!(
        canonical.contains("\"mag-eval\""),
        "ordinary child agents retain mag-eval for one-off world work"
    );
    fs::write(temp_root.join("canonical-agent.mag"), canonical)
        .expect("write exact canonical agent regression");
    let canonical_result = load(
        &mut reader,
        &mut stdin,
        "canonical-agent",
        &temp_root,
        Path::new("canonical-agent.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        canonical_result.get("kind").and_then(Value::as_str),
        Some("mag.loaded"),
        "the exact canonical patterns.md agent must compile against runtime contracts: {canonical_result:#?}"
    );

    // A library fragment may expose a useful subset of a foreign actor's
    // runtime outputs. Bash advertises mag.Text plus mag.CommandFailed; the
    // ordinary shell wrapper deliberately exposes only mag.Text. Unknown
    // authored wires remain invalid — the runtime inventory is authoritative.
    fs::write(
        temp_root.join("shell-output-subset.mag"),
        r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.shell")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      operation (nefor.shell.command "x" "true")
      result (nefor.graph.output-for "result" operation)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start operation)
         (nefor.graph.edge operation result)]))))"#,
    )
    .expect("write shell subset regression");
    let subset = load(
        &mut reader,
        &mut stdin,
        "shell-output-subset",
        &temp_root,
        Path::new("shell-output-subset.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        subset.get("kind").and_then(Value::as_str),
        Some("mag.loaded"),
        "a shell fragment exposing the mag.Text subset must compile: {subset:#?}"
    );

    let graph_laws = [
        (
            "direct",
            "(nefor.graph.add-edges base [first second])",
        ),
        (
            "permutation",
            "(nefor.graph.add-edges base [second first])",
        ),
        (
            "associative",
            "(nefor.graph.add-edges (nefor.graph.add-edges base [first]) [second])",
        ),
        (
            "idempotent",
            "(nefor.graph.add-edges (nefor.graph.add-edges base [first second]) [second first first])",
        ),
        (
            "absent-removal",
            "(nefor.graph.remove-edges (nefor.graph.add-edges base [first second]) [absent absent])",
        ),
        (
            "remove-add-roundtrip",
            "(nefor.graph.add-edges (nefor.graph.remove-edges (nefor.graph.add-edges base [first second]) [first]) [first])",
        ),
    ];
    let mut algebra_results = Vec::new();
    for (name, expression) in graph_laws {
        let file_name = format!("graph-edge-algebra-{name}.mag");
        let source = format!(
            r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.shell")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      operation (nefor.shell.command "operation" "true")
      unused (nefor.shell.command "unused" "false")
      result (nefor.graph.output-for "result" operation)
      first (nefor.graph.edge start operation)
      second (nefor.graph.edge operation result)
      absent (nefor.graph.edge start unused)
      expected (nefor.graph.graph [first second])
      laws [(= expected (nefor.graph.graph [second first]))
            (= expected (nefor.graph.add-edges
                          (nefor.graph.add-edges nefor.graph.empty-graph [first])
                          [second]))
            (= expected (nefor.graph.add-edges expected [first second first]))
            (= expected (nefor.graph.remove-edges expected [absent absent]))]
      laws-hold (= (count (filter (fn [[holds Bool]] -> Bool holds) laws))
                   (count laws))]
  (if laws-hold
    (nefor.artifact.compile
      (fn [[base nefor.graph.Graph]] -> nefor.graph.Graph
        {expression}))
    (fail {{:kind "GraphSetLawFailure"}})))"#
        );
        fs::write(temp_root.join(&file_name), source).expect("write graph edge algebra regression");
        let result = load(
            &mut reader,
            &mut stdin,
            &format!("graph-edge-algebra-{name}"),
            &temp_root,
            Path::new(&file_name),
            std::slice::from_ref(&lib_root),
        )
        .await;
        assert_eq!(
            result.get("kind").and_then(Value::as_str),
            Some("mag.loaded"),
            "pure graph set law {name} must compile: {result:#?}"
        );
        algebra_results.push((name, result));
    }
    let (_, algebra) = &algebra_results[0];
    let expected_artifact = algebra.get("artifact").expect("direct graph artifact");
    let expected_hash = algebra.get("hash").expect("direct graph artifact hash");
    for (name, result) in &algebra_results[1..] {
        assert_eq!(
            result.get("artifact"),
            Some(expected_artifact),
            "graph set law {name} changed canonical lowering"
        );
        assert_eq!(
            result.get("hash"),
            Some(expected_hash),
            "graph set law {name} changed the canonical artifact hash"
        );
    }
    let algebra_actors = algebra
        .get("artifact")
        .and_then(|artifact| artifact.pointer("/data/actors"))
        .and_then(Value::as_array)
        .expect("graph algebra artifact actors");
    assert_eq!(
        algebra_actors.len(),
        3,
        "duplicate additions collapse and absent removals introduce no nodes"
    );
    assert!(
        algebra_actors
            .iter()
            .all(|actor| actor.get("id").and_then(Value::as_str) != Some("unused")),
        "removing an absent edge must leave the graph unchanged"
    );
    let route_count = algebra_actors
        .iter()
        .filter_map(|actor| actor.get("routes").and_then(Value::as_object))
        .flat_map(|routes| routes.values())
        .filter_map(Value::as_array)
        .map(Vec::len)
        .sum::<usize>();
    assert_eq!(route_count, 2, "duplicate edges must not lower twice");

    fs::write(
        temp_root.join("worktree-create.mag"),
        r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.worktree")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      operation (nefor.worktree.create
                 "workspace"
                 (as nefor.worktree.CreateSpec
                   {:repository "/repo" :path "/worktrees/topic"
                    :branch "topic" :base "main"}))
      result (nefor.graph.output-for "result" operation)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start operation)
         (nefor.graph.edge operation result)]))))"#,
    )
    .expect("write worktree create regression");
    let worktree = load(
        &mut reader,
        &mut stdin,
        "worktree-create",
        &temp_root,
        Path::new("worktree-create.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        worktree.get("kind").and_then(Value::as_str),
        Some("mag.loaded"),
        "a native worktree fragment must compile against runtime contracts: {worktree:#?}"
    );
    let create_actor = worktree
        .get("artifact")
        .and_then(|artifact| artifact.pointer("/data/actors"))
        .and_then(Value::as_array)
        .and_then(|actors| {
            actors
                .iter()
                .find(|actor| actor.get("id").and_then(Value::as_str) == Some("workspace"))
        })
        .expect("worktree create actor is visible in the compiled preview");
    assert_eq!(
        create_actor.get("foreign").and_then(Value::as_str),
        Some("nefor.factory.worktree-create")
    );
    assert_eq!(
        create_actor.pointer("/params/repository"),
        Some(&json!("/repo"))
    );
    assert_eq!(
        create_actor.pointer("/params/path"),
        Some(&json!("/worktrees/topic"))
    );
    assert_eq!(
        create_actor.pointer("/params/branch"),
        Some(&json!("topic"))
    );
    assert_eq!(create_actor.pointer("/params/base"), Some(&json!("main")));
    assert!(
        create_actor.pointer("/params/mode").is_none(),
        "create and open remain distinct identities rather than a mode flag"
    );

    fs::write(
        temp_root.join("worktree-open.mag"),
        r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.worktree")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      operation (nefor.worktree.open
                 "workspace"
                 (as nefor.worktree.OpenSpec
                   {:repository "/repo" :path "/worktrees/topic"
                    :branch "topic"}))
      result (nefor.graph.output-for "result" operation)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start operation)
         (nefor.graph.edge operation result)]))))"#,
    )
    .expect("write worktree open regression");
    let open_worktree = load(
        &mut reader,
        &mut stdin,
        "worktree-open",
        &temp_root,
        Path::new("worktree-open.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        open_worktree.get("kind").and_then(Value::as_str),
        Some("mag.loaded"),
        "an explicit worktree open fragment must compile: {open_worktree:#?}"
    );
    let open_actor = open_worktree
        .get("artifact")
        .and_then(|artifact| artifact.pointer("/data/actors"))
        .and_then(Value::as_array)
        .and_then(|actors| {
            actors
                .iter()
                .find(|actor| actor.get("id").and_then(Value::as_str) == Some("workspace"))
        })
        .expect("worktree open actor");
    assert_eq!(
        open_actor.get("foreign").and_then(Value::as_str),
        Some("nefor.factory.worktree-open")
    );
    assert!(open_actor.pointer("/params/base").is_none());

    fs::write(
        temp_root.join("shell-output-unknown.mag"),
        r#"(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")
(let [input (nefor.graph.port "x" (type-tag Unit) "mag.Unit")
      output (nefor.graph.port "x" (type-tag nefor.contracts.Text) "mag.Unknown")
      actor (nefor.graph.actor
              "x"
              nefor.factory.bash
              (as nefor.contracts.BashParams {:command "true" :timeout_ms nil})
              (nefor.graph.store-port input)
              (as (List nefor.graph.StoredPort) [(nefor.graph.store-port output)]))
      operation (as (nefor.graph.Node Unit nefor.contracts.Text)
                 {:id "x" :role "ordinary"
                  :actors (as (List nefor.graph.Actor) [actor])
                  :routes (as (List nefor.graph.StoredRoute) [])
                  :messages (as (List nefor.graph.Message) [])
                  :input input
                  :output output})
      start (nefor.graph.source "start" (type-tag Unit) nil)
      result (nefor.graph.output-for "result" operation)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start operation)
         (nefor.graph.edge operation result)]))))"#,
    )
    .expect("write unknown shell output regression");
    let unknown = load(
        &mut reader,
        &mut stdin,
        "shell-output-unknown",
        &temp_root,
        Path::new("shell-output-unknown.mag"),
        std::slice::from_ref(&lib_root),
    )
    .await;
    assert_eq!(
        unknown.get("kind").and_then(Value::as_str),
        Some("mag.error"),
        "an authored output absent from the runtime scheme must fail: {unknown:#?}"
    );
    assert!(
        unknown
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("wire contract does not match")),
        "unknown output failure should identify the inventory contract: {unknown:#?}"
    );

    for (index, path) in entrypoints.iter().enumerate() {
        let entry = path
            .strip_prefix(&starter)
            .expect("entrypoint under starter");
        let result = load(
            &mut reader,
            &mut stdin,
            &format!("corpus-entry-{index}"),
            &starter,
            entry,
            std::slice::from_ref(&lib_root),
        )
        .await;
        assert_eq!(
            result.get("kind").and_then(Value::as_str),
            Some("mag.loaded"),
            "shipped MAG entrypoint {} failed to compile: {result:#?}",
            path.display()
        );
    }

    for (index, path) in fixtures.iter().enumerate() {
        let expected = fs::read_to_string(path.with_extension("error"))
            .unwrap_or_else(|error| panic!("read expectation for {}: {error}", path.display()));
        assert!(
            !expected.trim().is_empty(),
            "MAG failure fixture {} has an empty .error expectation",
            path.display()
        );
        let entry = path
            .strip_prefix(&fixture_root)
            .expect("fixture under fixture root");
        let result = load(
            &mut reader,
            &mut stdin,
            &format!("corpus-failure-{index}"),
            &fixture_root,
            entry,
            std::slice::from_ref(&lib_root),
        )
        .await;
        assert_eq!(
            result.get("kind").and_then(Value::as_str),
            Some("mag.error"),
            "expected MAG fixture {} to fail, got: {result:#?}",
            path.display()
        );
        let message = result
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "mag.error for {} has no message: {result:#?}",
                    path.display()
                )
            });
        assert!(
            message.contains(expected.trim()),
            "MAG fixture {} produced unexpected diagnostic\nexpected substring: {:?}\nactual: {message}",
            path.display(),
            expected.trim()
        );
    }

    shutdown(stdin, child).await;
    let _ = fs::remove_dir_all(temp_root);
}
