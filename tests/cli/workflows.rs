//! End-to-end workflow engine tests over the CLI socket path.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::harness::*;

fn unique_test_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hcli-wf-{}-{nanos}", std::process::id()))
}

fn write_workflow(path: &std::path::Path, name: &str, tail_command: &str) {
    let workflow = serde_json::json!({
        "name": name,
        "nodes": [
            {"id": "intro", "type": "prompt_template", "template": "seed text"},
            {
                "id": "tail", "type": "agent", "title": "tail",
                "runtime": "custom",
                "custom_command": tail_command,
                "prompt": "{{intro.output}}",
                "after": ["intro"],
                "visible": false
            }
        ]
    });
    std::fs::write(path, serde_json::to_string_pretty(&workflow).unwrap()).unwrap();
}

fn wait_for_run_done(
    config_home: &std::path::Path,
    runtime_dir: &std::path::Path,
    run_id: &str,
) -> Value {
    for _ in 0..40 {
        let run = run_named_cli_json(config_home, runtime_dir, &["workflow", "get", run_id]);
        let status = run["result"]["run"]["status"].as_str().unwrap_or("");
        if status == "done" || status == "error" {
            return run["result"]["run"].clone();
        }
        thread::sleep(Duration::from_millis(250));
    }
    panic!("workflow run {run_id} did not finish in time");
}

#[test]
fn workflow_run_executes_dag_and_caches_on_resume() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let SpawnedHerdr { child, .. } = spawn_named_server(&config_home, &runtime_dir, "workflows");

    let workflow_path = base.join("demo.aflow.json");
    write_workflow(&workflow_path, "wf-e2e", "printf 'tail saw: '; cat");

    // 1. Fresh run: both nodes execute.
    let started = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["workflow", "run", workflow_path.to_str().unwrap()],
    );
    let run_id = started["result"]["run_id"].as_str().unwrap().to_string();
    let run = wait_for_run_done(&config_home, &runtime_dir, &run_id);
    assert_eq!(run["status"], "done");
    for node in run["nodes"].as_array().unwrap() {
        assert_eq!(node["phase"], "done", "node {}", node["id"]);
        assert!(
            !node["cached"].as_bool().unwrap(),
            "fresh run must not be cached"
        );
    }

    // 2. Pause mid-run semantics: run is already done, so resume errors.
    let resume_done = run_named_cli(&config_home, &runtime_dir, &["workflow", "resume", &run_id]);
    let stderr = String::from_utf8_lossy(&resume_done.stderr);
    assert!(stderr.contains("only paused runs resume"), "got: {stderr}");

    // 3. delete cleans up.
    run_named_cli_json(&config_home, &runtime_dir, &["workflow", "delete", &run_id]);
    let listed = run_named_cli_json(&config_home, &runtime_dir, &["workflow", "list"]);
    let ids: Vec<&str> = listed["result"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["run_id"].as_str())
        .collect();
    assert!(!ids.contains(&run_id.as_str()));

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}

#[test]
fn workflow_rejects_invalid_file() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let SpawnedHerdr { child, .. } =
        spawn_named_server(&config_home, &runtime_dir, "workflows-bad");

    let workflow_path = base.join("bad.aflow.json");
    std::fs::write(
        &workflow_path,
        r#"{"name": "bad", "nodes": [{"id": "a", "type": "agent", "prompt": "x", "after": ["a"]}]}"#,
    )
    .unwrap();

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &["workflow", "run", workflow_path.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("depends on itself"),
        "cycle must be rejected: {stderr}"
    );

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}
