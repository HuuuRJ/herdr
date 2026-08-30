//! Agent-node command construction, shell wrapping, and output parsing.
//!
//! Two execution shapes share this module:
//!
//! - **visible** nodes run in a pane through a platform shell wrapper that
//!   mirrors output into a log file (`tee` on Unix, `Tee-Object` on Windows)
//!   and appends a `HERDR_EXIT:<code>` sentinel. The pane dies when the
//!   command finishes; the engine later reads exit code and output from the
//!   file, which sidesteps both "PaneDied carries no exit code" and the
//!   pane-teardown race.
//! - **invisible** nodes run the raw argv (or shell string) as a background
//!   process with piped stdout; the process layer owns exit codes.
//!
//! Injection channels (base URL, key, model) follow the field-tested
//! AgentFlow FR-8.4 mappings.

use std::path::Path;

use crate::api::schema::{ProviderProfile, ProviderProtocol};
use crate::workflow::model::{AgentRuntime, PermissionLevel, WorkflowNode};

pub(crate) const EXIT_SENTINEL: &str = "HERDR_EXIT:";

// -- command shapes ----------------------------------------------------------

/// What an agent node executes.
#[derive(Debug)]
pub(crate) enum AgentCommand {
    /// Structured argv (claude/codex): never passes through a shell unless
    /// wrapped for a visible pane.
    Argv(Vec<String>),
    /// A raw shell command string (custom runtime).
    ShellString(String),
}

/// Build the agent command for a node. `prompt` must already be rendered;
/// `prompt_file` is where the App layer staged the prompt text (grok-build
/// reads it via `--prompt-file`, sidestepping every argv quoting hazard).
pub(crate) fn build_agent_command(
    node: &WorkflowNode,
    prompt: &str,
    profile: Option<&ProviderProfile>,
    sanitized_profile_key: Option<&str>,
    prompt_file: &Path,
) -> Result<AgentCommand, String> {
    match node.runtime {
        Some(AgentRuntime::ClaudeCode) => {
            // Model selection rides env only (ANTHROPIC_MODEL + tier
            // alignment in provider_env): relays reject the request shape
            // the CLI sends with an argv --model (field-verified against
            // moonshot's anthropic gateway: 400 modelCode with --model,
            // success env-only), and env is what AgentFlow FR-8.4 specified.
            let mut argv = vec![
                "claude".to_string(),
                "-p".to_string(),
                prompt.to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if let Some(max_turns) = node.max_turns {
                argv.push("--max-turns".to_string());
                argv.push(max_turns.to_string());
            }
            if let Some(permission) = node.permission {
                argv.push("--permission-mode".to_string());
                argv.push(permission.claude_flag().to_string());
            }
            Ok(AgentCommand::Argv(argv))
        }
        Some(AgentRuntime::Codex) => {
            let mut argv = vec![
                "codex".to_string(),
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
            ];
            if let Some(permission) = node.permission {
                argv.extend(permission.codex_args().iter().map(|flag| flag.to_string()));
            }
            if let Some((provider_args, _)) = codex_provider_config(profile, sanitized_profile_key)
            {
                argv.extend(provider_args);
            }
            if let Some(model) = effective_model(node, profile) {
                argv.push("-m".to_string());
                argv.push(model.to_string());
            }
            argv.push(prompt.to_string());
            Ok(AgentCommand::Argv(argv))
        }
        Some(AgentRuntime::GrokBuild) => {
            require_openai_compat(profile, "grok-build")?;
            let mut argv = vec![
                "grok".to_string(),
                "--prompt-file".to_string(),
                prompt_file.to_string_lossy().into_owned(),
                "--output-format".to_string(),
                "streaming-messages-json".to_string(),
            ];
            if let Some(model) = effective_model(node, profile) {
                argv.push("-m".to_string());
                argv.push(model.to_string());
            }
            if let Some(max_turns) = node.max_turns {
                argv.push("--max-turns".to_string());
                argv.push(max_turns.to_string());
            }
            if let Some(permission) = node.permission {
                // FR-8.3 grok column uses the same mode names as claude.
                argv.push("--permission-mode".to_string());
                argv.push(permission.claude_flag().to_string());
            }
            Ok(AgentCommand::Argv(argv))
        }
        Some(AgentRuntime::Dsh) => {
            require_openai_compat(profile, "dsh")?;
            // dsh joins task words on one line; fold newlines like custom.
            let folded = prompt.replace("\r\n", " ").replace('\n', " ");
            if folded.trim().is_empty() {
                return Err("dsh task prompt is empty".to_string());
            }
            if cfg!(windows) {
                // dsh resolves to an npm .cmd shim; task text through cmd.exe
                // argv would expose cmd metacharacters. Ride base64 inside
                // PowerShell instead, fold stderr into plain text (mixed
                // CLIXML/text streams break a PowerShell parent), and
                // propagate the exit code.
                let script = format!(
                    "& dsh --profile headless {} 2>&1 | Out-String; exit $LASTEXITCODE",
                    crate::platform::quote_powershell_arg(&folded)
                );
                Ok(AgentCommand::Argv(powershell_encoded_argv(&script)))
            } else {
                Ok(AgentCommand::Argv(vec![
                    "dsh".to_string(),
                    "--profile".to_string(),
                    "headless".to_string(),
                    folded,
                ]))
            }
        }
        Some(AgentRuntime::Custom) => {
            let template = node
                .custom_command
                .as_deref()
                .unwrap_or_default()
                .to_string();
            let mut expanded = template;
            if let Some(model) = effective_model(node, profile) {
                expanded = expanded.replace("{{model}}", model);
            }
            // Newlines inside the substituted prompt would break the
            // surrounding command string (and .cmd shims on Windows treat
            // them as command separators); fold to single-line.
            let folded_prompt = prompt.replace("\r\n", " ").replace('\n', " ");
            expanded = expanded.replace("{{prompt}}", &folded_prompt);
            if expanded.trim().is_empty() {
                return Err("custom command expanded to nothing".to_string());
            }
            Ok(AgentCommand::ShellString(expanded))
        }
        None => Err("agent node has no runtime".to_string()),
    }
}

pub(crate) fn effective_model<'a>(
    node: &'a WorkflowNode,
    profile: Option<&'a ProviderProfile>,
) -> Option<&'a str> {
    node.model.as_deref().or_else(|| {
        profile
            .and_then(|profile| profile.models.iter().find(|m| m.visible))
            .map(|model| model.id.as_str())
    })
}

/// grok-build and dsh talk to openai-compat endpoints only (FR-8.2); a
/// bound profile of any other protocol is a configuration error.
fn require_openai_compat(profile: Option<&ProviderProfile>, runtime: &str) -> Result<(), String> {
    if let Some(profile) = profile {
        if profile.protocol != ProviderProtocol::OpenaiCompat {
            return Err(format!(
                "{runtime} requires an openai-compat provider profile (bound profile uses {:?})",
                profile.protocol
            ));
        }
    }
    Ok(())
}

// -- provider → env bridge ---------------------------------------------------

/// Environment variables a bound provider profile injects for the runtime.
/// Keys never appear in argv; visible panes get them via `extra_env`,
/// background processes via `Command::envs`.
pub(crate) fn provider_env(
    profile: &ProviderProfile,
    sanitized_profile_key: Option<&str>,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    match profile.protocol {
        ProviderProtocol::Anthropic => {
            env.push(("ANTHROPIC_BASE_URL".to_string(), profile.base_url.clone()));
            env.push(("ANTHROPIC_AUTH_TOKEN".to_string(), profile.api_key.clone()));
            if let Some(model) = model {
                env.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
                // Relays reject unknown default models; align every tier so
                // claude never falls back to a model the relay lacks.
                env.push((
                    "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                    model.to_string(),
                ));
                env.push((
                    "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                    model.to_string(),
                ));
                env.push((
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                    model.to_string(),
                ));
                env.push(("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), model.to_string()));
            }
        }
        ProviderProtocol::OpenaiCompat => {
            if let Some(key_name) = sanitized_profile_key {
                env.push((format!("AF_KEY_{key_name}"), profile.api_key.clone()));
            }
            // Generic vars for custom commands ({{env:OPENAI_API_KEY}}).
            env.push(("OPENAI_BASE_URL".to_string(), profile.base_url.clone()));
            env.push(("OPENAI_API_KEY".to_string(), profile.api_key.clone()));
        }
        ProviderProtocol::Gemini => {
            env.push(("GEMINI_API_KEY".to_string(), profile.api_key.clone()));
        }
    }
    env
}

/// codex provider wiring: `-c model_providers.*` argv plus the env var name
/// that will carry the key (`(args, env_key_name)`).
pub(crate) fn codex_provider_config(
    profile: Option<&ProviderProfile>,
    sanitized_profile_key: Option<&str>,
) -> Option<(Vec<String>, String)> {
    let profile = profile?;
    let key_name = sanitized_profile_key?;
    if profile.protocol != ProviderProtocol::OpenaiCompat {
        return None;
    }
    let provider_id = format!("af_{key_name}");
    let env_key = format!("AF_KEY_{key_name}");
    let args = vec![
        "-c".to_string(),
        format!("model_providers.{provider_id}.name=herdr-{key_name}"),
        "-c".to_string(),
        format!(
            "model_providers.{provider_id}.base_url={}",
            profile.base_url
        ),
        "-c".to_string(),
        format!("model_providers.{provider_id}.env_key={env_key}"),
        "-c".to_string(),
        format!("model_provider={provider_id}"),
        "-c".to_string(),
        format!("model_providers.{provider_id}.wire_api=responses"),
    ];
    Some((args, env_key))
}

/// Expand `{{env:VAR}}` placeholders in a custom command from the injected
/// provider environment.
pub(crate) fn expand_env_placeholders(command: &str, env: &[(String, String)]) -> String {
    let mut result = command.to_string();
    for (key, value) in env {
        result = result.replace(&format!("{{{{env:{key}}}}}"), value);
    }
    result
}

/// grok-build env injection (FR-8.4): everything rides env; `home_dir` (a
/// per-node temp home) keeps `~/.grok` untouched and must already exist.
pub(crate) fn grok_env(profile: &ProviderProfile, home_dir: &Path) -> Vec<(String, String)> {
    vec![
        ("GROK_MODELS_BASE_URL".to_string(), profile.base_url.clone()),
        ("XAI_API_KEY".to_string(), profile.api_key.clone()),
        (
            "GROK_HOME".to_string(),
            home_dir.to_string_lossy().into_owned(),
        ),
        ("GROK_DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
    ]
}

/// dsh env injection: the key under the env-var name that settings.yaml
/// references, plus the permission mode (workspace tier sets nothing).
pub(crate) fn dsh_env(
    profile: &ProviderProfile,
    sanitized_profile_key: &str,
    permission: Option<PermissionLevel>,
    home_dir: &Path,
) -> Vec<(String, String)> {
    let mut env = vec![
        (
            format!("AF_KEY_{sanitized_profile_key}"),
            profile.api_key.clone(),
        ),
        // Without DSH_HOME the CLI reads ~/.dsh and never sees the staged
        // settings.yaml (field-verified: MISSING_CREDENTIAL with the file
        // sitting unused in the node directory).
        (
            "DSH_HOME".to_string(),
            home_dir.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(mode) = permission.and_then(PermissionLevel::dsh_env) {
        env.push(("DSH_PERMISSION_MODE".to_string(), mode.to_string()));
    }
    env
}

/// settings.yaml body for the per-node temp `DSH_HOME` (schema verified
/// against dsh 0.1.1-rc.2: `llm-<ns>` provider blocks override the built-in
/// deepseek provider; `agent-default-model` points the headless boot at the
/// injected catalog entry).
pub(crate) fn dsh_settings_yaml(
    profile: &ProviderProfile,
    sanitized_profile_key: &str,
    model: Option<&str>,
) -> String {
    fn yaml_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }
    let mut yaml = format!(
        "llm-deepseek:\n  apiKeyEnv: AF_KEY_{sanitized_profile_key}\n  baseURL: {}\n",
        yaml_quote(&profile.base_url)
    );
    if let Some(model) = model {
        yaml.push_str(&format!(
            "  models:\n    - id: {}\n      name: {}\n      contextWindow: 128000\n",
            yaml_quote(model),
            yaml_quote(model)
        ));
        yaml.push_str(&format!(
            "agent-default-model:\n  provider: deepseek-official\n  model: {}\n",
            yaml_quote(model)
        ));
    }
    yaml
}

/// `[a-z0-9_]` profile-key slug used in env var and codex provider names.
pub(crate) fn sanitized_profile_key(profile_id: &str) -> String {
    let mut slug: String = profile_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    slug.truncate(24);
    slug
}

// -- visible shell wrapping --------------------------------------------------

/// POSIX single-quote wrapper (shared with the image viewer pane command).
pub(crate) fn unix_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_line(command: &AgentCommand) -> String {
    match command {
        AgentCommand::Argv(argv) => argv
            .iter()
            .map(|part| unix_shell_quote(part))
            .collect::<Vec<_>>()
            .join(" "),
        AgentCommand::ShellString(string) => string.clone(),
    }
}

/// Unix visible wrapper: pure POSIX, no pipefail needed — the sentinel rides
/// the command's stderr into the same pipe tee consumes.
fn unix_wrap(command: &AgentCommand, log_path: &Path) -> Vec<String> {
    let quoted_log = unix_shell_quote(&log_path.to_string_lossy());
    let script = format!(
        "{{ {}; echo {}$?; }} 2>&1 | tee {quoted_log}",
        command_line(command),
        EXIT_SENTINEL
    );
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// PowerShell script body for the Windows visible wrapper. Exposed for
/// tests; encoding happens in `windows_wrap`.
fn windows_tee_script(command: &AgentCommand, log_path: &Path) -> String {
    let log = crate::platform::quote_powershell_arg(&log_path.to_string_lossy());
    let invocation = match command {
        AgentCommand::Argv(argv) => {
            let (program, args) = argv.split_first().expect("non-empty argv");
            let mut line = format!("& {}", crate::platform::quote_powershell_arg(program));
            for arg in args {
                line.push(' ');
                line.push_str(&crate::platform::quote_powershell_arg(arg));
            }
            line
        }
        AgentCommand::ShellString(string) => string.clone(),
    };
    format!(
        // UTF-8 first: PowerShell decodes native child output with the OEM
        // codepage (e.g. GBK on zh-CN Windows) before Tee re-encodes it, so
        // UTF-8 children come out mojibake'd without this.
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
         {invocation} 2>&1 | Tee-Object -FilePath {log}; \
         $code = if ($LASTEXITCODE -ne $null) {{ $LASTEXITCODE }} elseif ($) {{ 0 }} else {{ 1 }}; \
         \"{EXIT_SENTINEL}$code\" | Add-Content -Encoding Unicode {log}"
    )
}

/// Windows visible wrapper: `-EncodedCommand` sidesteps every quoting
/// hazard (the whole script, prompt included, rides base64 UTF-16).
fn windows_wrap(command: &AgentCommand, log_path: &Path) -> Vec<String> {
    powershell_encoded_argv(&windows_tee_script(command, log_path))
}

/// `powershell.exe -EncodedCommand <base64 UTF-16 script>` argv. The script
/// body never touches a command line, so quoting hazards cannot exist.
/// No NUL terminator: a trailing U+0000 stays in the decoded script and
/// breaks nested -EncodedCommand parsing (field-verified: `Unexpected
/// token '\0'`).
pub(crate) fn powershell_encoded_argv(script: &str) -> Vec<String> {
    use base64::Engine as _;
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    vec![
        "powershell.exe".to_string(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-EncodedCommand".to_string(),
        base64::engine::general_purpose::STANDARD.encode(utf16),
    ]
}

/// Wrap an agent command for a visible pane on the current platform.
pub(crate) fn shell_wrap_visible(command: &AgentCommand, log_path: &Path) -> Vec<String> {
    if cfg!(windows) {
        windows_wrap(command, log_path)
    } else {
        unix_wrap(command, log_path)
    }
}

// -- output parsing ----------------------------------------------------------

/// Final values extracted from a node's captured output.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct NodeOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<u64>,
}

/// Decode raw log bytes (UTF-8 or UTF-16LE with BOM — PowerShell Tee writes
/// UTF-16), strip the exit sentinel, and extract the runtime's final answer.
pub(crate) fn parse_node_output(raw: &[u8], runtime: AgentRuntime) -> NodeOutput {
    let decoded = decode_log(raw);
    let (body, exit_code) = split_sentinel(&decoded);

    let mut output = match runtime {
        // grok's streaming-messages-json IS the Anthropic wire format, so it
        // reuses the claude extraction (result line carries cost/usage).
        AgentRuntime::ClaudeCode | AgentRuntime::GrokBuild => match parse_claude_stream(&body) {
            Some(mut parsed) => {
                // The sentinel exit code wins when present; an is_error
                // result keeps its failure code otherwise.
                parsed.exit_code = exit_code.or(parsed.exit_code);
                return parsed;
            }
            None => parse_lenient_last_json_text(&body).unwrap_or_else(|| body.trim().to_string()),
        },
        AgentRuntime::Codex => {
            parse_lenient_last_json_text(&body).unwrap_or_else(|| body.trim().to_string())
        }
        // dsh headless prints the final assistant message as plain text.
        AgentRuntime::Dsh | AgentRuntime::Custom => body.trim().to_string(),
    };
    if output.is_empty() {
        output = body.trim().to_string();
    }
    NodeOutput {
        text: output,
        exit_code,
        model: None,
        cost_usd: None,
        tokens: None,
    }
}

fn decode_log(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

/// Split off the trailing `HERDR_EXIT:<n>` line: `(body, code)`.
fn split_sentinel(decoded: &str) -> (String, Option<i32>) {
    let mut body = decoded.to_string();
    let mut exit_code = None;
    if let Some(position) = body.rfind(EXIT_SENTINEL) {
        let tail = &body[position + EXIT_SENTINEL.len()..];
        let code_text = tail
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('\u{feff}');
        if let Ok(code) = code_text.parse::<i32>() {
            exit_code = Some(code);
            body = body[..position]
                .trim_end_matches(['\r', '\n', '\u{feff}'])
                .to_string();
        }
    }
    (body, exit_code)
}

/// claude `-p --output-format stream-json`: the last `{"type":"result"}`
/// line carries `result`, `total_cost_usd`, and `usage`.
fn parse_claude_stream(body: &str) -> Option<NodeOutput> {
    for line in body.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(|kind| kind.as_str()) != Some("result") {
            continue;
        }
        let tokens = value
            .get("usage")
            .and_then(|usage| {
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                (input + output > 0).then_some(input + output)
            })
            .or_else(|| {
                value
                    .get("num_turns")
                    .and_then(|turns| turns.as_u64())
                    .filter(|turns| *turns > 0)
            });
        // An error result line (claude/grok set is_error with an errors
        // array) must fail the node, not complete it with an empty output.
        let is_error = value
            .get("is_error")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);
        let text = if is_error {
            // grok puts the reason in `errors[]`; claude puts it in the
            // `result` field of an error result line. Either way the node
            // error must carry the real reason.
            let from_errors = value
                .get("errors")
                .and_then(|errors| errors.as_array())
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(|error| error.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|joined| !joined.is_empty());
            from_errors
                .or_else(|| {
                    value
                        .get("result")
                        .and_then(|result| result.as_str())
                        .filter(|result| !result.is_empty())
                        .map(|result| result.to_string())
                })
                .unwrap_or_else(|| "agent reported an execution error".to_string())
        } else {
            value
                .get("result")
                .and_then(|result| result.as_str())
                .unwrap_or("")
                .to_string()
        };
        return Some(NodeOutput {
            text,
            // An error result fails the node even when the exit sentinel is
            // missing (PowerShell shim failures can swallow it).
            exit_code: is_error.then_some(1),
            model: value
                .get("model")
                .and_then(|model| model.as_str())
                .map(|model| model.to_string()),
            cost_usd: value.get("total_cost_usd").and_then(|cost| cost.as_f64()),
            tokens,
        });
    }
    None
}

/// Lenient extractor for JSON-lines output (codex, unknown runtimes): take
/// a text-ish field from the last parseable JSON line.
fn parse_lenient_last_json_text(body: &str) -> Option<String> {
    for line in body.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        for key in ["result", "text", "message", "output"] {
            if let Some(text) = value.get(key).and_then(|field| field.as_str()) {
                if !text.trim().is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::NodeType;
    use std::collections::HashMap;

    fn agent_node(json: &str) -> WorkflowNode {
        let base = format!(r#"{{"id": "n", "type": "agent"{json}}}"#);
        serde_json::from_str(&base).unwrap()
    }

    fn profile(protocol: ProviderProtocol, base_url: &str, key: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".to_string(),
            name: "test".to_string(),
            preset_id: "custom".to_string(),
            protocol,
            base_url: base_url.to_string(),
            api_key: key.to_string(),
            models: vec![],
            weight: 1,
            is_disabled: false,
            note: None,
            created_unix: 0,
        }
    }

    #[test]
    fn unix_wrap_quotes_and_appends_sentinel() {
        let command = AgentCommand::Argv(vec![
            "claude".to_string(),
            "-p".to_string(),
            "multi\nline prompt's".to_string(),
        ]);
        let argv = unix_wrap(&command, Path::new("/tmp/run/nodes/a/log.jsonl"));
        assert_eq!(argv[..2], ["/bin/sh".to_string(), "-c".to_string()]);
        let script = &argv[2];
        assert!(script.starts_with("{ claude -p '"));
        assert!(script.contains("HERDR_EXIT:$?"));
        assert!(script.ends_with("tee /tmp/run/nodes/a/log.jsonl"));
        // Newlines survive inside single quotes; apostrophes are escaped.
        assert!(script.contains("multi\nline prompt"));
        assert!(script.contains("prompt'\\''s"));
    }

    #[test]
    fn windows_script_tees_and_appends_exit() {
        let command = AgentCommand::Argv(vec![
            "claude".to_string(),
            "-p".to_string(),
            "hi there".to_string(),
        ]);
        let script = windows_tee_script(&command, Path::new("C:\\run\\log.jsonl"));
        assert!(script.contains("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8"));
        assert!(script.contains("& claude -p 'hi there'"));
        assert!(script.contains("Tee-Object -FilePath"));
        assert!(script.contains("Add-Content -Encoding Unicode"));
        assert!(script.contains("HERDR_EXIT:$code"));
    }

    #[test]
    fn windows_wrap_is_encoded_command() {
        let command = AgentCommand::ShellString("echo hi".to_string());
        let argv = windows_wrap(&command, Path::new("C:/t/log"));
        assert_eq!(argv[0], "powershell.exe");
        assert_eq!(argv[3], "-EncodedCommand");
        // Decoding round-trips.
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(argv[4].as_bytes())
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let script = String::from_utf16_lossy(&units);
        assert!(script.contains("echo hi"));
        assert!(script.contains("Tee-Object"));
    }

    #[test]
    fn parses_utf8_log_with_sentinel() {
        let raw = format!(
            "partial line\n{{\"type\":\"result\",\"result\":\"final answer\"}}\n{EXIT_SENTINEL}0\n"
        );
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::ClaudeCode);
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.text, "final answer");
    }

    #[test]
    fn parses_utf16_log_from_powershell() {
        let body = "plain custom output\r\n";
        let full = format!("{body}{EXIT_SENTINEL}2\r\n");
        let mut raw: Vec<u8> = vec![0xFF, 0xFE];
        raw.extend(full.encode_utf16().flat_map(|unit| unit.to_le_bytes()));
        let output = parse_node_output(&raw, AgentRuntime::Custom);
        assert_eq!(output.exit_code, Some(2));
        assert_eq!(output.text, "plain custom output");
    }

    #[test]
    fn claude_result_carries_cost_and_tokens() {
        let raw = concat!(
            "{\"type\":\"assistant\",\"message\":{}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done text\",\"total_cost_usd\":0.42,\"usage\":{\"input_tokens\":10,\"output_tokens\":5},\"model\":\"claude-x\"}\n",
            "HERDR_EXIT:0\n"
        );
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::ClaudeCode);
        assert_eq!(output.text, "done text");
        assert_eq!(output.cost_usd, Some(0.42));
        assert_eq!(output.tokens, Some(15));
        assert_eq!(output.model.as_deref(), Some("claude-x"));
    }

    #[test]
    fn grok_output_reuses_claude_extraction() {
        let raw = concat!(
            "{\"type\":\"assistant\",\"message\":{}}\n",
            "{\"type\":\"result\",\"result\":\"grok done\",\"total_cost_usd\":0.1,\"usage\":{\"input_tokens\":4,\"output_tokens\":6},\"model\":\"grok-4\"}\n",
            "HERDR_EXIT:0\n"
        );
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::GrokBuild);
        assert_eq!(output.text, "grok done");
        assert_eq!(output.cost_usd, Some(0.1));
        assert_eq!(output.tokens, Some(10));
        assert_eq!(output.model.as_deref(), Some("grok-4"));
    }

    #[test]
    fn error_result_line_fails_the_node_even_without_sentinel() {
        // Real grok 1.0.13 shape: not-signed-in ends the stream with an
        // is_error result and a nonzero process exit.
        let raw = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"total_cost_usd\":0.0,\"errors\":[\"Not signed in. Run `grok login` to authenticate.\"],\"model\":\"unknown\"}\n"
        );
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::GrokBuild);
        assert_eq!(output.exit_code, Some(1));
        assert!(output.text.contains("Not signed in"), "{}", output.text);
        // With a zero sentinel, the sentinel still wins (clean exit).
        let raw_ok = concat!(
            "{\"type\":\"result\",\"is_error\":true,\"errors\":[\"x\"],\"result\":\"\"}\n",
            "HERDR_EXIT:0\n"
        );
        let output = parse_node_output(raw_ok.as_bytes(), AgentRuntime::GrokBuild);
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn unparseable_output_falls_back_to_full_text() {
        let raw = format!("just words\nmore words\n{EXIT_SENTINEL}0\n");
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::ClaudeCode);
        assert_eq!(output.text, "just words\nmore words");
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn codex_lenient_json_extraction() {
        let raw = format!(
            "{{\"type\":\"item.started\"}}\n{{\"type\":\"item.completed\",\"text\":\"codex final\"}}\n{EXIT_SENTINEL}0\n"
        );
        let output = parse_node_output(raw.as_bytes(), AgentRuntime::Codex);
        assert_eq!(output.text, "codex final");
    }

    #[test]
    fn anthropic_profile_env_aligns_all_tiers() {
        let profile = profile(
            ProviderProtocol::Anthropic,
            "https://relay.example.com",
            "sk-k",
        );
        let env = provider_env(&profile, None, Some("glm-4.7"));
        let map: HashMap<_, _> = env.iter().cloned().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://relay.example.com")
        );
        assert_eq!(
            map.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("sk-k")
        );
        assert_eq!(
            map.get("ANTHROPIC_MODEL").map(String::as_str),
            Some("glm-4.7")
        );
        assert_eq!(
            map.get("ANTHROPIC_DEFAULT_OPUS_MODEL").map(String::as_str),
            Some("glm-4.7")
        );
        assert_eq!(
            map.get("CLAUDE_CODE_SUBAGENT_MODEL").map(String::as_str),
            Some("glm-4.7")
        );
    }

    #[test]
    fn codex_provider_config_builds_minus_c_chain() {
        let openai_profile = profile(
            ProviderProtocol::OpenaiCompat,
            "https://api.example.com/v1",
            "sk-o",
        );
        let (args, env_key) = codex_provider_config(Some(&openai_profile), Some("p1")).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("model_providers.af_p1.name=herdr-p1"));
        assert!(joined.contains("model_providers.af_p1.base_url=https://api.example.com/v1"));
        assert!(joined.contains("model_providers.af_p1.env_key=AF_KEY_p1"));
        assert!(joined.contains("wire_api=responses"));
        assert_eq!(env_key, "AF_KEY_p1");
        // Anthropic profiles are rejected for codex.
        let anthropic_profile = profile(ProviderProtocol::Anthropic, "https://x", "k");
        assert!(codex_provider_config(Some(&anthropic_profile), Some("p1")).is_none());
    }

    #[test]
    fn custom_command_expansion_folds_prompt_newlines() {
        let node = agent_node(
            r#", "runtime": "custom", "custom_command": "run.sh {{model}} -- {{prompt}}", "model": "m1""#,
        );
        assert_eq!(node.node_type, NodeType::Agent);
        let command =
            build_agent_command(&node, "line1\nline2", None, None, Path::new("/tmp/p.txt"))
                .unwrap();
        match command {
            AgentCommand::ShellString(string) => {
                assert_eq!(string, "run.sh m1 -- line1 line2");
            }
            _ => panic!("expected shell string"),
        }
    }

    #[test]
    fn grok_command_uses_prompt_file_and_streaming_messages() {
        let node = agent_node(
            r#", "runtime": "grok-build", "prompt": "p", "model": "grok-4", "max_turns": 5, "permission": "full""#,
        );
        let command = build_agent_command(
            &node,
            "task text",
            None,
            None,
            Path::new("/run/nodes/g/prompt.txt"),
        )
        .unwrap();
        match command {
            AgentCommand::Argv(argv) => {
                let joined = argv.join(" ");
                assert!(joined.contains("--prompt-file /run/nodes/g/prompt.txt"));
                assert!(joined.contains("--output-format streaming-messages-json"));
                assert!(joined.contains("-m grok-4"));
                assert!(joined.contains("--max-turns 5"));
                assert!(joined.contains("--permission-mode bypassPermissions"));
            }
            _ => panic!("expected argv"),
        }
    }

    #[test]
    fn grok_rejects_non_openai_compat_profile() {
        let node = agent_node(r#", "runtime": "grok-build", "prompt": "p""#);
        let anthropic = profile(ProviderProtocol::Anthropic, "https://x", "k");
        let err = build_agent_command(
            &node,
            "task",
            Some(&anthropic),
            None,
            Path::new("/tmp/p.txt"),
        )
        .unwrap_err();
        assert!(err.contains("openai-compat"), "{err}");
    }

    #[test]
    fn dsh_command_rides_safe_channel() {
        let node = agent_node(r#", "runtime": "dsh", "prompt": "p""#);
        let command = build_agent_command(
            &node,
            "run the tests & fix | things",
            None,
            None,
            Path::new("/tmp/p.txt"),
        )
        .unwrap();
        match command {
            AgentCommand::Argv(argv) if cfg!(windows) => {
                // The task text must ride base64, never a cmd.exe command line.
                assert_eq!(argv[0], "powershell.exe");
                assert_eq!(argv[3], "-EncodedCommand");
                assert!(!argv.iter().any(|part| part.contains("fix |")));
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(argv[4].as_bytes())
                    .unwrap();
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                let script = String::from_utf16_lossy(&units);
                assert!(script.contains("dsh --profile headless"));
                assert!(script.contains("run the tests & fix | things"));
                assert!(script.contains("exit $LASTEXITCODE"));
            }
            AgentCommand::Argv(argv) => {
                assert_eq!(argv[..3], ["dsh", "--profile", "headless"]);
                assert_eq!(argv[3], "run the tests & fix | things");
            }
            _ => panic!("expected argv"),
        }
    }

    #[test]
    fn grok_env_and_dsh_env_mappings() {
        let openai = profile(
            ProviderProtocol::OpenaiCompat,
            "https://api.x.ai/v1",
            "sk-x",
        );
        let grok = grok_env(&openai, Path::new("/run/nodes/g/.af-grok-home"));
        let grok_map: HashMap<_, _> = grok.iter().cloned().collect();
        assert_eq!(
            grok_map.get("GROK_MODELS_BASE_URL").map(String::as_str),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(
            grok_map.get("XAI_API_KEY").map(String::as_str),
            Some("sk-x")
        );
        assert_eq!(
            grok_map.get("GROK_HOME").map(String::as_str),
            Some("/run/nodes/g/.af-grok-home")
        );
        assert_eq!(
            grok_map.get("GROK_DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );

        let dsh = dsh_env(
            &openai,
            "p1",
            Some(PermissionLevel::Full),
            Path::new("/run/nodes/d/.af-dsh-home"),
        );
        let dsh_map: HashMap<_, _> = dsh.iter().cloned().collect();
        assert_eq!(dsh_map.get("AF_KEY_p1").map(String::as_str), Some("sk-x"));
        assert_eq!(
            dsh_map.get("DSH_PERMISSION_MODE").map(String::as_str),
            Some("danger-full-access")
        );
        // Workspace tier sets nothing (headless default is workspace-write).
        let workspace = dsh_env(
            &openai,
            "p1",
            Some(PermissionLevel::Workspace),
            Path::new("/run/nodes/d/.af-dsh-home"),
        );
        assert!(workspace
            .iter()
            .all(|(key, _)| key != "DSH_PERMISSION_MODE"));
        assert!(dsh
            .iter()
            .any(|(key, value)| key == "DSH_HOME" && value == "/run/nodes/d/.af-dsh-home"));
    }

    #[test]
    fn dsh_settings_yaml_shape() {
        let openai = profile(
            ProviderProtocol::OpenaiCompat,
            "https://relay.example.com/v1",
            "sk-x",
        );
        let yaml = dsh_settings_yaml(&openai, "p1", Some("glm-4.7"));
        assert!(yaml.contains("apiKeyEnv: AF_KEY_p1"), "{yaml}");
        assert!(yaml.contains("baseURL: 'https://relay.example.com/v1'"));
        assert!(yaml.contains("id: 'glm-4.7'"));
        assert!(yaml.contains("agent-default-model:"));
        assert!(yaml.contains("model: 'glm-4.7'"));
        // No model: no catalog/default-model overrides.
        let bare = dsh_settings_yaml(&openai, "p1", None);
        assert!(!bare.contains("agent-default-model"), "{bare}");
    }

    #[test]
    fn env_placeholder_expansion() {
        let env = vec![
            ("OPENAI_API_KEY".to_string(), "sk-x".to_string()),
            (
                "OPENAI_BASE_URL".to_string(),
                "https://api.example.com/v1".to_string(),
            ),
        ];
        assert_eq!(
            expand_env_placeholders(
                "curl {{env:OPENAI_BASE_URL}} -H {{env:OPENAI_API_KEY}}",
                &env
            ),
            "curl https://api.example.com/v1 -H sk-x"
        );
    }

    #[test]
    fn profile_key_slug_is_lowercase_alnum() {
        assert_eq!(sanitized_profile_key("p1788006477x1"), "p1788006477x1");
        assert_eq!(sanitized_profile_key("Weird-ID.99"), "weird_id_99");
    }
}
