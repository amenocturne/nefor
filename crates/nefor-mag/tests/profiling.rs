use nefor_mag::profile::CompileProfiler;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("nefor-mag-{label}-{nonce}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

#[test]
fn profiled_load_reports_phases_and_deterministic_work() {
    let root = temp_dir("profile");
    fs::write(
        root.join("library.mag"),
        "(def copy (fn [[value Int]] -> Int value))",
    )
    .expect("library");
    fs::write(
        root.join("main.mag"),
        "(require \"library\")\n(artifact \"test.profile/v1\" {:value (library.copy 7)})",
    )
    .expect("entry");

    let profiler = CompileProfiler::new();
    let program = nefor_mag::load_with_profiler(
        &root,
        "main.mag",
        json!({}),
        std::slice::from_ref(&root),
        &profiler,
    )
    .expect("profiled load");
    nefor_mag::validate_loaded_rules_profiled(&program, &profiler).expect("rules");
    let profile = profiler.snapshot();

    assert_eq!(profile.counters.module_requests, 1);
    assert_eq!(profile.counters.modules_loaded, 1);
    assert_eq!(profile.counters.module_cache_hits, 0);
    assert!(profile.counters.evaluator_steps > 0);
    assert!(profile.counters.function_calls > 0);
    assert!(profile.counters.environment_snapshots > 0);
    assert!(profile.phases.entry_read_ns > 0);
    assert!(profile.phases.entry_lex_ns > 0);
    assert!(profile.phases.entry_parse_ns > 0);
    assert!(profile.phases.entry_evaluate_ns > 0);
    assert!(profile.phases.module_resolve_ns > 0);
    assert!(profile.phases.module_read_ns > 0);
    assert!(profile.phases.module_lex_ns > 0);
    assert!(profile.phases.module_parse_ns > 0);
    assert!(profile.phases.module_evaluate_ns > 0);
    assert!(profile.phases.checking_ns > 0);
    assert!(profile.phases.artifact_serialize_hash_ns > 0);

    fs::remove_dir_all(root).ok();
}
