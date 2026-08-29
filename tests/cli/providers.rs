//! End-to-end provider profile tests over the CLI socket path.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use serde_json::Value;

use super::harness::*;

fn create_profile(
    config_home: &Path,
    runtime_dir: &Path,
    name: &str,
    base_url: &str,
    key: &str,
) -> Value {
    run_named_cli_json(
        config_home,
        runtime_dir,
        &[
            "provider",
            "create",
            "--name",
            name,
            "--protocol",
            "openai-compat",
            "--base-url",
            base_url,
            "--key",
            key,
        ],
    )
}

#[test]
fn provider_crud_round_trip_masks_and_preserves_key() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let SpawnedHerdr { child, .. } = spawn_named_server(&config_home, &runtime_dir, "providers");

    let created = create_profile(
        &config_home,
        &runtime_dir,
        "relay one",
        "https://api.example.com/v1",
        "sk-roundtrip-1234567890",
    );
    let profile = &created["result"]["profile"];
    let profile_id = profile["id"].as_str().unwrap().to_string();
    assert_eq!(profile["api_key_masked"], "sk-***7890");
    assert!(profile["has_api_key"].as_bool().unwrap());

    // List is masked.
    let listed = run_named_cli_json(&config_home, &runtime_dir, &["provider", "list"]);
    assert_eq!(
        listed["result"]["profiles"][0]["api_key_masked"],
        "sk-***7890"
    );

    // Reveal returns the plaintext.
    let revealed = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "reveal", &profile_id],
    );
    assert_eq!(
        revealed["result"]["api_key"].as_str().unwrap(),
        "sk-roundtrip-1234567890"
    );

    // Update without --key keeps the stored key.
    let updated = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "update", &profile_id, "--note", "renamed"],
    );
    assert_eq!(updated["result"]["profile"]["note"], "renamed");
    let revealed_again = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "reveal", &profile_id],
    );
    assert_eq!(
        revealed_again["result"]["api_key"].as_str().unwrap(),
        "sk-roundtrip-1234567890"
    );

    // Delete removes it.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "delete", &profile_id],
    );
    let listed = run_named_cli_json(&config_home, &runtime_dir, &["provider", "list"]);
    assert_eq!(listed["result"]["profiles"].as_array().unwrap().len(), 0);

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}

#[test]
fn named_sessions_share_provider_registry() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let SpawnedHerdr { child, .. } = spawn_named_server(&config_home, &runtime_dir, "providers-a");
    let created = create_profile(
        &config_home,
        &runtime_dir,
        "shared profile",
        "https://api.example.com/v1",
        "sk-shared-abcdef123456",
    );
    let profile_id = created["result"]["profile"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = child.kill();
    let _ = child.wait();

    // A second server over the same config home sees the same registry.
    let SpawnedHerdr { child, .. } = spawn_named_server(&config_home, &runtime_dir, "providers-b");
    let listed = run_named_cli_json(&config_home, &runtime_dir, &["provider", "list"]);
    assert_eq!(
        listed["result"]["profiles"][0]["id"].as_str().unwrap(),
        profile_id
    );

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}

/// Minimal HTTP/1.1 responder speaking the OpenAI-compatible `/models`
/// shape, used to exercise the deferred model-fetch path without external
/// network.
struct MockProvider {
    base_url: String,
}

impl MockProvider {
    fn start(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                // Read until the end of the headers (our requests are
                // body-less GETs).
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.extend_from_slice(&chunk[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{addr}"),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

#[test]
fn provider_models_fetch_merges_via_deferred_curl() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let SpawnedHerdr { child, .. } =
        spawn_named_server(&config_home, &runtime_dir, "providers-models");

    let mock = MockProvider::start(r#"{"data": [{"id": "model-a"}, {"id": "model-b"}]}"#);
    let created = create_profile(
        &config_home,
        &runtime_dir,
        "mock relay",
        &mock.base_url(),
        "sk-mock-0987654321",
    );
    let profile_id = created["result"]["profile"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fetched = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "models", &profile_id],
    );
    let models = fetched["result"]["result"]["models"].as_array().unwrap();
    let ids: Vec<&str> = models
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"model-a"));
    assert!(ids.contains(&"model-b"));

    // The merged list is persisted into the profile.
    let listed = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["provider", "get", &profile_id],
    );
    assert_eq!(
        listed["result"]["profile"]["models"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}

#[test]
fn provider_test_reports_auth_failure_from_mock() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let SpawnedHerdr { child, .. } =
        spawn_named_server(&config_home, &runtime_dir, "providers-test");

    // The mock answers every request (including the chat probe) with 401.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(_) => break,
            };
            let mut chunk = [0u8; 1024];
            let _ = stream.read(&mut chunk);
            let body = r#"{"error": {"message": "bad key"}}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let created = create_profile(
        &config_home,
        &runtime_dir,
        "auth fail",
        &base_url,
        "sk-bad-key",
    );
    let profile_id = created["result"]["profile"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &["provider", "test", &profile_id],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["result"]["result"]["http_status"].as_u64(),
        Some(401),
        "expected 401 classification, got: {parsed}"
    );
    assert_eq!(parsed["result"]["result"]["ok"].as_bool(), Some(false));

    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_base(base);
}
