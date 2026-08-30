//! Workflow engine glue — drives the pure engine against real panes and
//! background processes.
//!
//! Every run owns a workspace; visible agent nodes are tabs in it (argv =
//! platform shell wrapper teeing into the run directory), invisible nodes
//! run as background processes with piped output. Completion for visible
//! nodes arrives as `AppEvent::PaneDied` (the log file carries the exit
//! sentinel); invisible nodes report through `AppEvent::WorkflowNodeFinished`.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Instant;

use crate::app::state::{Mode, WorkflowGraphSnapshot, WorkflowNodeView, WorkflowRunSummary};
use crate::events::{AppEvent, WorkflowNodeFinished};
use crate::layout::PaneId;
use crate::workflow::engine::EngineRun;
use crate::workflow::executors::agent::{
    build_agent_command, dsh_env, dsh_settings_yaml, effective_model, expand_env_placeholders,
    grok_env, parse_node_output, powershell_encoded_argv, provider_env, sanitized_profile_key,
    shell_wrap_visible, unix_shell_quote, AgentCommand,
};
use crate::workflow::executors::image::run_image_gen;
use crate::workflow::executors::template::render;
use crate::workflow::graph;
use crate::workflow::model::{AgentRuntime, NodeType, WorkflowDef, WorkflowNode};
use crate::workflow::runs::{self, NodeMeta, NodePhase, RunRecord, RunStatus};

/// Live per-run execution state (engine + pane/process bindings).
pub(crate) struct WorkflowRunLive {
    pub engine: EngineRun,
    pub workspace_idx: usize,
    pub started_unix: u64,
    pub pane_of_node: HashMap<String, PaneId>,
    /// Image-viewer panes. Kept OUT of `pane_of_node` so their death is not
    /// consumed as node completion (`handle_workflow_pane_died`).
    pub viewer_pane_of_node: HashMap<String, PaneId>,
    pub pid_of_node: HashMap<String, std::sync::Arc<std::sync::Mutex<Option<u32>>>>,
    pub deadline_of_node: HashMap<String, Instant>,
}

const MAX_INVISIBLE_OUTPUT_BYTES: usize = 1024 * 1024;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn node_log_path(run_id: &str, node_id: &str) -> PathBuf {
    runs::node_root(run_id, node_id).join("log.txt")
}

impl crate::app::App {
    // -- lifecycle -----------------------------------------------------------

    pub(crate) fn start_workflow_run(&mut self, path: &str) -> Result<String, String> {
        // Resolve once against the server cwd: CLI clients send paths
        // relative to their own shell, and the stored workflow_path is
        // re-read on resume (it must survive either cwd).
        let path = std::path::absolute(path)
            .map_err(|err| format!("invalid workflow path {path}: {err}"))?
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read workflow file {path}: {err}"))?;
        let def = WorkflowDef::parse(&text)?;
        let run_id = runs::generate_run_id();
        let run_root = runs::run_root(&run_id);
        std::fs::create_dir_all(run_root.join("nodes"))
            .map_err(|err| format!("failed to create run directory: {err}"))?;

        let workspace_idx = self
            .create_workspace_with_launch_env(run_root.clone(), false, Vec::new())
            .map_err(|err| format!("failed to create workflow workspace: {err}"))?;
        let workspace_id = self
            .state
            .workspaces
            .get(workspace_idx)
            .map(|ws| ws.id.clone());

        let mut engine = EngineRun::new(def, run_id.clone(), path.to_string());
        engine.workspace_id = workspace_id;
        let started_unix = now_unix();
        runs::create_run(&engine.record(started_unix, None))
            .map_err(|err| format!("failed to persist run: {err}"))?;

        self.workflow_runs.insert(
            run_id.clone(),
            WorkflowRunLive {
                engine,
                workspace_idx,
                started_unix,
                pane_of_node: HashMap::new(),
                viewer_pane_of_node: HashMap::new(),
                pid_of_node: HashMap::new(),
                deadline_of_node: HashMap::new(),
            },
        );
        self.label_workflow_workspace(workspace_idx, &run_id);
        self.dispatch_workflow_ready_nodes();
        self.refresh_workflow_view();
        Ok(run_id)
    }

    fn label_workflow_workspace(&mut self, workspace_idx: usize, run_id: &str) {
        let name = self
            .workflow_runs
            .get(run_id)
            .map(|live| format!("wf:{}", live.engine.def.name))
            .unwrap_or_else(|| "wf".to_string());
        if let Some(workspace) = self.state.workspaces.get_mut(workspace_idx) {
            workspace.set_custom_name(name);
        }
    }

    pub(crate) fn pause_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        let Some(live) = self.workflow_runs.get_mut(run_id) else {
            return Err(format!("run {run_id} not found or not live"));
        };
        live.engine.pause();
        self.persist_workflow_run(run_id);
        self.refresh_workflow_view();
        Ok(())
    }

    pub(crate) fn resume_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        // A paused run still holds live state in THIS process: resume in
        // place so pane/process bindings and deadlines survive and
        // still-Running nodes are never re-spawned. The record rebuild below
        // is only for runs recovered after a server restart.
        if let Some(mut live) = self.workflow_runs.remove(run_id) {
            if live.engine.status != RunStatus::Paused {
                let status = live.engine.status;
                self.workflow_runs.insert(run_id.to_string(), live);
                return Err(format!(
                    "run {run_id} is {} ; only paused or errored runs resume",
                    status.as_str()
                ));
            }
            let text = std::fs::read_to_string(&live.engine.workflow_path)
                .map_err(|err| format!("failed to re-read workflow file: {err}"))?;
            let def = WorkflowDef::parse(&text)?;
            live.engine.def = def;
            live.engine.resume();
            reset_errored_nodes(&mut live.engine);
            live.engine
                .invalidate_stale_done_nodes(|node_id, config_hash, inputs_hash| {
                    runs::load_cached_node(run_id, node_id, config_hash, inputs_hash).is_some()
                });
            self.workflow_runs.insert(run_id.to_string(), live);
            self.persist_workflow_run(run_id);
            self.dispatch_workflow_ready_nodes();
            self.refresh_workflow_view();
            return Ok(());
        }

        let Some(record) = runs::load_record(run_id) else {
            return Err(format!("run {run_id} not found"));
        };
        if !matches!(record.status, RunStatus::Paused | RunStatus::Error) {
            return Err(format!(
                "run {run_id} is {} ; only paused or errored runs resume",
                record.status.as_str()
            ));
        }
        let text = std::fs::read_to_string(&record.workflow_path)
            .map_err(|err| format!("failed to re-read workflow file: {err}"))?;
        let def = WorkflowDef::parse(&text)?;

        // Rebuild live state: reuse the workspace when it still exists,
        // otherwise create a fresh one under the same run directory.
        let workspace_idx = match self
            .state
            .workspaces
            .iter()
            .position(|ws| Some(&ws.id) == record.workspace_id.as_ref())
        {
            Some(idx) => idx,
            None => {
                let idx = self
                    .create_workspace_with_launch_env(runs::run_root(run_id), false, Vec::new())
                    .map_err(|err| format!("failed to recreate workflow workspace: {err}"))?;
                if let Some(workspace) = self.state.workspaces.get(idx) {
                    let id = workspace.id.clone();
                    if let Some(live) = self.workflow_runs.get_mut(run_id) {
                        live.engine.workspace_id = Some(id);
                    }
                }
                idx
            }
        };

        let mut engine = EngineRun::from_record(&record, def);
        engine.resume();
        // Anything marked Running in the record but not actually bound to a
        // pane/process (server restarted mid-run) must restart.
        let stale: Vec<String> = engine
            .nodes
            .iter()
            .filter(|(_, node)| node.phase == NodePhase::Running)
            .map(|(id, _)| id.clone())
            .collect();
        for node_id in stale {
            if let Some(node) = engine.nodes.get_mut(&node_id) {
                node.phase = NodePhase::Pending;
            }
        }
        // Failed nodes re-run on resume (W9 fix-and-rerun).
        reset_errored_nodes(&mut engine);
        // Done nodes edited while the run was paused (or stranded by an
        // upstream reset) cascade back to Pending; unchanged ones stay Done
        // and the cache reloads their outputs below.
        engine.invalidate_stale_done_nodes(|node_id, config_hash, inputs_hash| {
            runs::load_cached_node(run_id, node_id, config_hash, inputs_hash).is_some()
        });

        let started_unix = record.started_unix;
        self.workflow_runs.insert(
            run_id.to_string(),
            WorkflowRunLive {
                engine,
                workspace_idx,
                started_unix,
                pane_of_node: HashMap::new(),
                viewer_pane_of_node: HashMap::new(),
                pid_of_node: HashMap::new(),
                deadline_of_node: HashMap::new(),
            },
        );
        self.apply_workflow_cache(run_id);
        self.persist_workflow_run(run_id);
        self.dispatch_workflow_ready_nodes();
        self.refresh_workflow_view();
        Ok(())
    }

    pub(crate) fn cancel_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        let Some(live) = self.workflow_runs.get_mut(run_id) else {
            // Not live: just flip the persisted record if it exists.
            let mut record =
                runs::load_record(run_id).ok_or_else(|| format!("run {run_id} not found"))?;
            if record.status.is_terminal() {
                return Ok(());
            }
            record.status = RunStatus::Cancelled;
            record.finished_unix = Some(now_unix());
            let _ = runs::save_record(&record);
            self.refresh_workflow_view();
            return Ok(());
        };
        // Kill running visible panes through the runtime registry.
        for (_, pane_id) in live.pane_of_node.iter() {
            let terminal_id = self
                .state
                .workspaces
                .get(live.workspace_idx)
                .and_then(|ws| ws.terminal_id(*pane_id))
                .cloned();
            if let Some(terminal_id) = terminal_id {
                if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
                    runtime.shutdown();
                }
            }
        }
        // Kill invisible children by pid tree.
        for (_, pid_slot) in live.pid_of_node.iter() {
            if let Some(pid) = pid_slot.lock().expect("pid lock").take() {
                let pids = crate::platform::session_processes(pid);
                crate::platform::signal_processes(&pids, crate::platform::Signal::Terminate);
            }
        }
        live.engine.cancel();
        self.finish_workflow_run(run_id, RunStatus::Cancelled);
        self.refresh_workflow_view();
        Ok(())
    }

    pub(crate) fn delete_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        if self.workflow_runs.contains_key(run_id) {
            self.cancel_workflow_run(run_id)?;
        } else if let Some(record) = runs::load_record(run_id) {
            // A done/error run keeps its workspace until delete (W10).
            self.close_workspace(&record.workspace_id);
        }
        runs::delete_run(run_id).map_err(|err| err.to_string())?;
        self.refresh_workflow_view();
        Ok(())
    }

    fn close_workspace(&mut self, workspace_id: &Option<String>) {
        self.close_workflow_workspace(workspace_id, usize::MAX);
    }

    // -- dispatch -------------------------------------------------------------

    fn workflow_agent_in_flight(&self) -> usize {
        self.workflow_runs
            .values()
            .flat_map(|live| {
                live.engine
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.phase == NodePhase::Running)
                    .filter(move |(id, _)| {
                        live.engine
                            .def
                            .node(id)
                            .is_some_and(|node| node.node_type == NodeType::Agent)
                    })
                    .map(|_| ())
            })
            .count()
    }

    pub(crate) fn dispatch_workflow_ready_nodes(&mut self) {
        let max_concurrent = self.config_workflow_max_agents();
        let mut dispatched_any = false;
        let run_ids: Vec<String> = self.workflow_runs.keys().cloned().collect();
        for run_id in run_ids {
            loop {
                let in_flight = self.workflow_agent_in_flight();
                let ready = {
                    let Some(live) = self.workflow_runs.get(&run_id) else {
                        break;
                    };
                    live.engine.ready_nodes(in_flight, max_concurrent)
                };
                let Some(node_id) = ready.first().cloned() else {
                    break;
                };
                if !self.dispatch_workflow_node(&run_id, &node_id) {
                    break;
                }
                dispatched_any = true;
            }
        }
        if dispatched_any {
            self.schedule_session_save();
        }
    }

    fn config_workflow_max_agents(&self) -> usize {
        self.workflow_limits.max_concurrent_agents.max(1)
    }

    /// Dispatch one node; returns false when dispatch must stop (run left
    /// the map or the node failed synchronously).
    fn dispatch_workflow_node(&mut self, run_id: &str, node_id: &str) -> bool {
        let Some(node) = self
            .workflow_runs
            .get(run_id)
            .and_then(|live| live.engine.def.node(node_id).cloned())
        else {
            return false;
        };
        // Structurally skipped (disabled, or downstream of a disabled node):
        // empty output port, no spawn, no concurrency slot.
        if self
            .workflow_runs
            .get(run_id)
            .is_some_and(|live| live.engine.def.is_structurally_skipped(&node.id))
        {
            self.complete_workflow_node(run_id, &node.id, String::new(), &NodeMeta::default());
            return true;
        }
        match node.node_type {
            NodeType::PromptTemplate => self.dispatch_template_node(run_id, &node),
            NodeType::ImageGen => self.dispatch_image_node(run_id, &node),
            NodeType::Agent => self.dispatch_agent_node(run_id, &node),
        }
    }

    fn dispatch_template_node(&mut self, run_id: &str, node: &WorkflowNode) -> bool {
        let rendered = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return false;
            };
            render(node.template.as_deref().unwrap_or(""), &live.engine.outputs)
        };
        match rendered {
            Ok(text) => {
                self.complete_workflow_node(run_id, &node.id, text, &NodeMeta::default());
                true
            }
            Err(err) => {
                self.fail_workflow_node(run_id, &node.id, err);
                false
            }
        }
    }

    fn dispatch_image_node(&mut self, run_id: &str, node: &WorkflowNode) -> bool {
        let profile = self.workflow_profile_for(run_id, node);
        let Some(profile) = profile else {
            self.fail_workflow_node(
                run_id,
                &node.id,
                "image_gen requires a bound openai-compat provider profile".to_string(),
            );
            return false;
        };
        let prompt = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return false;
            };
            match render(node.prompt.as_deref().unwrap_or(""), &live.engine.outputs) {
                Ok(prompt) => prompt,
                Err(err) => {
                    self.fail_workflow_node(run_id, &node.id, err);
                    return false;
                }
            }
        };
        let output_path = runs::node_root(run_id, &node.id)
            .join(node.output_file.as_deref().unwrap_or("image.png"));
        if node.visible {
            // A viewer pane for the generated image (I2): it prints the
            // deterministic artifact path, then idles until the run ends.
            self.spawn_workflow_viewer_pane(run_id, &node.id, &output_path);
        }
        let size = node.size.clone();
        let model = node.model.clone();
        let node_id = node.id.clone();
        let run_id = run_id.to_string();
        if let Some(live) = self.workflow_runs.get_mut(&run_id) {
            live.engine.mark_running(&node_id);
        }

        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = run_image_gen(
                &profile,
                &prompt,
                size.as_deref(),
                model.as_deref(),
                &output_path,
            )
            .map(|artifact| {
                let meta = NodeMeta {
                    artifact: Some(artifact.clone()),
                    ..Default::default()
                };
                (artifact, meta)
            });
            let _ = event_tx.blocking_send(AppEvent::WorkflowNodeFinished(Box::new(
                WorkflowNodeFinished {
                    run_id,
                    node_id,
                    result,
                },
            )));
        });
        true
    }

    fn workflow_profile_for(
        &self,
        run_id: &str,
        node: &WorkflowNode,
    ) -> Option<crate::api::schema::ProviderProfile> {
        let profile_id = node.provider_profile_id.as_deref()?;
        crate::persist::provider_registry::load()
            .into_iter()
            .find(|profile| profile.id == profile_id)
            .or_else(|| {
                tracing::warn!(run_id, profile_id, "workflow node bound to missing profile");
                None
            })
    }

    fn dispatch_agent_node(&mut self, run_id: &str, node: &WorkflowNode) -> bool {
        let profile = self.workflow_profile_for(run_id, node);
        if node.provider_profile_id.is_some() && profile.is_none() {
            self.fail_workflow_node(
                run_id,
                &node.id,
                format!(
                    "provider profile '{}' not found",
                    node.provider_profile_id.as_deref().unwrap_or("?")
                ),
            );
            return false;
        }
        let prompt = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return false;
            };
            match render(node.prompt.as_deref().unwrap_or(""), &live.engine.outputs) {
                Ok(prompt) => prompt,
                Err(err) => {
                    self.fail_workflow_node(run_id, &node.id, err);
                    return false;
                }
            }
        };
        let sanitized_key = profile
            .as_ref()
            .map(|profile| sanitized_profile_key(&profile.id));
        let _runtime = node.runtime.expect("validated agent runtime");
        // The shell wrapper / prompt file / temp homes all live in the node
        // directory; it must exist before spawn (Tee-Object fails silently
        // otherwise).
        let node_root = runs::node_root(run_id, &node.id);
        let _ = std::fs::create_dir_all(&node_root);
        let prompt_file = node_root.join("prompt.txt");
        if _runtime == AgentRuntime::GrokBuild {
            if let Err(err) = std::fs::write(&prompt_file, &prompt) {
                self.fail_workflow_node(
                    run_id,
                    &node.id,
                    format!("failed to stage prompt file: {err}"),
                );
                return false;
            }
        }
        let command = match build_agent_command(
            node,
            &prompt,
            profile.as_ref(),
            sanitized_key.as_deref(),
            &prompt_file,
        ) {
            Ok(command) => command,
            Err(err) => {
                self.fail_workflow_node(run_id, &node.id, err);
                return false;
            }
        };
        let mut env = profile
            .as_ref()
            .map(|profile| provider_env(profile, sanitized_key.as_deref(), node.model.as_deref()))
            .unwrap_or_default();
        // Runtime-specific injection (FR-8.4): per-node temp homes keep the
        // user's global grok/dsh config untouched (A6).
        if let Some(profile) = profile.as_ref() {
            match _runtime {
                AgentRuntime::GrokBuild => {
                    let home = node_root.join(".af-grok-home");
                    let _ = std::fs::create_dir_all(&home);
                    env.extend(grok_env(profile, &home));
                }
                AgentRuntime::Dsh => {
                    let home = node_root.join(".af-dsh-home");
                    let _ = std::fs::create_dir_all(&home);
                    let model = effective_model(node, Some(profile));
                    let _ = std::fs::write(
                        home.join("settings.yaml"),
                        dsh_settings_yaml(
                            profile,
                            sanitized_key.as_deref().unwrap_or_default(),
                            model,
                        ),
                    );
                    env.extend(dsh_env(
                        profile,
                        sanitized_key.as_deref().unwrap_or_default(),
                        node.permission,
                    ));
                }
                _ => {}
            }
        }
        let model = node.model.clone();
        let command = match command {
            // Custom templates may reference injected vars via {{env:VAR}}.
            AgentCommand::ShellString(string) => {
                AgentCommand::ShellString(expand_env_placeholders(&string, &env))
            }
            other => other,
        };

        if let Some(live) = self.workflow_runs.get_mut(run_id) {
            live.engine.mark_running(&node.id);
            if node.timeout_ms > 0 {
                live.deadline_of_node.insert(
                    node.id.clone(),
                    Instant::now() + std::time::Duration::from_millis(node.timeout_ms),
                );
            }
        }

        if node.visible {
            self.spawn_workflow_agent_pane(run_id, node, command, env)
        } else {
            self.spawn_workflow_agent_process(run_id, node, command, env, model);
            true
        }
    }

    fn spawn_workflow_agent_pane(
        &mut self,
        run_id: &str,
        node: &WorkflowNode,
        command: AgentCommand,
        env: Vec<(String, String)>,
    ) -> bool {
        let log_path = node_log_path(run_id, &node.id);
        let argv = shell_wrap_visible(&command, &log_path);
        let (workspace_idx, cwd, scrollback, theme, appearance) = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return false;
            };
            (
                live.workspace_idx,
                runs::run_root(run_id),
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
            )
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let created = self.state.workspaces.get_mut(workspace_idx).and_then(|ws| {
            ws.create_tab_argv_command(rows, cols, cwd, &argv, env, scrollback, theme, appearance)
                .ok()
        });
        let Some((tab_idx, terminal, runtime)) = created else {
            self.fail_workflow_node(
                run_id,
                &node.id,
                "failed to spawn workflow pane".to_string(),
            );
            return false;
        };
        let pane_id = self.state.workspaces[workspace_idx].tabs[tab_idx].root_pane;
        let terminal_id = terminal.id.clone();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        if let Some(existing) = self.state.terminals.get_mut(&terminal_id) {
            existing.set_manual_label(node.display_title().to_string());
        }
        if let Some(live) = self.workflow_runs.get_mut(run_id) {
            live.pane_of_node.insert(node.id.clone(), pane_id);
        }
        true
    }

    /// Viewer pane for a visible image_gen node: prints the artifact path
    /// (the no-graphics fallback text), then sleeps until the run ends.
    /// Registered in `viewer_pane_of_node` — NOT `pane_of_node` — so pane
    /// death never reads as node completion.
    fn spawn_workflow_viewer_pane(
        &mut self,
        run_id: &str,
        node_id: &str,
        output_path: &std::path::Path,
    ) {
        let path_text = output_path.to_string_lossy().into_owned();
        let message = format!("image -> {path_text}");
        let argv = if cfg!(windows) {
            powershell_encoded_argv(&format!(
                "Write-Host {}; Start-Sleep -Seconds 86400",
                crate::platform::quote_powershell_arg(&message)
            ))
        } else {
            vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '%s\\n' {}; sleep 86400", unix_shell_quote(&message)),
            ]
        };
        let (workspace_idx, cwd, scrollback, theme, appearance) = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return;
            };
            (
                live.workspace_idx,
                runs::run_root(run_id),
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
            )
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let created = self.state.workspaces.get_mut(workspace_idx).and_then(|ws| {
            ws.create_tab_argv_command(
                rows,
                cols,
                cwd,
                &argv,
                Vec::new(),
                scrollback,
                theme,
                appearance,
            )
            .ok()
        });
        let Some((tab_idx, terminal, runtime)) = created else {
            tracing::warn!(run_id, node_id, "failed to spawn workflow viewer pane");
            return;
        };
        let pane_id = self.state.workspaces[workspace_idx].tabs[tab_idx].root_pane;
        let terminal_id = terminal.id.clone();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        if let Some(existing) = self.state.terminals.get_mut(&terminal_id) {
            let title = self
                .workflow_runs
                .get(run_id)
                .and_then(|live| live.engine.def.node(node_id))
                .map(|node| node.display_title().to_string())
                .unwrap_or_else(|| node_id.to_string());
            existing.set_manual_label(title);
        }
        if let Some(live) = self.workflow_runs.get_mut(run_id) {
            live.viewer_pane_of_node
                .insert(node_id.to_string(), pane_id);
        }
    }

    fn spawn_workflow_agent_process(
        &mut self,
        run_id: &str,
        node: &WorkflowNode,
        command: AgentCommand,
        env: Vec<(String, String)>,
        model: Option<String>,
    ) {
        let run_id = run_id.to_string();
        let node_id = node.id.clone();
        let _runtime = node.runtime.expect("validated agent runtime");
        let cwd = runs::run_root(&run_id);
        let pid_slot: std::sync::Arc<std::sync::Mutex<Option<u32>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        if let Some(live) = self.workflow_runs.get_mut(&run_id) {
            live.pid_of_node.insert(node_id.clone(), pid_slot.clone());
        }
        let event_tx = self.event_tx.clone();

        std::thread::spawn(move || {
            let mut command = match &command {
                AgentCommand::Argv(argv) => {
                    let (program, args) = argv.split_first().expect("non-empty argv");
                    crate::plugin_command::command_for_argv_in_dir(program, args, &cwd)
                }
                AgentCommand::ShellString(string) => shell_string_command(string),
            };
            command.envs(env.clone()).current_dir(&cwd);
            let mut child = match command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    let _ = event_tx.blocking_send(AppEvent::WorkflowNodeFinished(Box::new(
                        WorkflowNodeFinished {
                            run_id,
                            node_id,
                            result: Err(format!("failed to spawn: {err}")),
                        },
                    )));
                    return;
                }
            };
            *pid_slot.lock().expect("pid lock") = Some(child.id());

            let stdout_pipe = child.stdout.take();
            let stderr_pipe = child.stderr.take();
            let stdout = read_capped(stdout_pipe, MAX_INVISIBLE_OUTPUT_BYTES);
            let stderr = read_capped(stderr_pipe, MAX_INVISIBLE_OUTPUT_BYTES);
            let status = child.wait();
            *pid_slot.lock().expect("pid lock") = None;

            let mut bytes = stdout;
            if bytes.is_empty() {
                bytes = stderr;
            } else {
                bytes.push(b'\n');
                bytes.extend_from_slice(&stderr);
            }
            let mut output = parse_node_output(&bytes, _runtime);
            if output.model.is_none() {
                output.model = model;
            }
            let exit_code = match status {
                Ok(status) => status.code(),
                Err(_) => Some(-1),
            };
            let effective_exit = output.exit_code.or(exit_code);
            let result = if effective_exit.is_some_and(|code| code != 0) {
                // Carry the parsed error text (e.g. an is_error result line)
                // alongside the bare code so failures stay actionable.
                let detail = output.text.trim();
                Err(if detail.is_empty() {
                    format!("process exited with code {}", effective_exit.unwrap_or(-1))
                } else {
                    format!(
                        "process exited with code {}: {detail}",
                        effective_exit.unwrap_or(-1)
                    )
                })
            } else {
                Ok((
                    output.text.clone(),
                    NodeMeta {
                        exit_code: effective_exit,
                        model: output.model.clone(),
                        cost_usd: output.cost_usd,
                        tokens: output.tokens,
                        ..Default::default()
                    },
                ))
            };
            let _ = event_tx.blocking_send(AppEvent::WorkflowNodeFinished(Box::new(
                WorkflowNodeFinished {
                    run_id,
                    node_id,
                    result,
                },
            )));
        });
    }

    // -- completion ----------------------------------------------------------

    /// Engine hook for `AppEvent::PaneDied`: when the pane belongs to a
    /// workflow node, consume it here. Returns true when consumed.
    pub(crate) fn handle_workflow_pane_died(&mut self, pane_id: PaneId) -> bool {
        let Some((run_id, node_id)) = self.workflow_runs.iter().find_map(|(run_id, live)| {
            live.pane_of_node
                .iter()
                .find(|(_, pid)| **pid == pane_id)
                .map(|(node_id, _)| (run_id.clone(), node_id.clone()))
        }) else {
            return false;
        };
        let Some(live) = self.workflow_runs.get_mut(&run_id) else {
            return false;
        };
        live.pane_of_node.remove(&node_id);
        live.deadline_of_node.remove(&node_id);
        let (runtime, _timeout_ms) = live
            .engine
            .def
            .node(&node_id)
            .map(|node| (node.runtime, node.timeout_ms))
            .unwrap_or((None, 0));

        let log_path = node_log_path(&run_id, &node_id);
        let raw = std::fs::read(&log_path).unwrap_or_default();
        let output = parse_node_output(
            &raw,
            runtime.unwrap_or(crate::workflow::model::AgentRuntime::Custom),
        );
        let exit_ok = output.exit_code.is_none_or(|code| code == 0);
        if exit_ok {
            self.complete_workflow_node_with(
                &run_id,
                &node_id,
                output.text,
                NodeMeta {
                    exit_code: output.exit_code,
                    model: output.model,
                    cost_usd: output.cost_usd,
                    tokens: output.tokens,
                    ..Default::default()
                },
            );
        } else {
            let detail = output.text.trim();
            let message = if detail.is_empty() {
                format!(
                    "process exited with code {}",
                    output.exit_code.unwrap_or(-1)
                )
            } else {
                format!(
                    "process exited with code {}: {detail}",
                    output.exit_code.unwrap_or(-1)
                )
            };
            self.fail_workflow_node(&run_id, &node_id, message);
        }
        self.refresh_workflow_view();
        true
    }

    pub(crate) fn handle_workflow_node_finished(&mut self, event: Box<WorkflowNodeFinished>) {
        let WorkflowNodeFinished {
            run_id,
            node_id,
            result,
        } = *event;
        // Capture viewer-pane + artifact before completion may finish the
        // run and drop the live state (I2 pane projection happens after).
        let viewer_pane = self
            .workflow_runs
            .get(&run_id)
            .and_then(|live| live.viewer_pane_of_node.get(&node_id).copied());
        let artifact = result
            .as_ref()
            .ok()
            .and_then(|(_, meta)| meta.artifact.clone());
        if let Some(live) = self.workflow_runs.get_mut(&run_id) {
            live.pid_of_node.remove(&node_id);
            live.deadline_of_node.remove(&node_id);
        }
        match result {
            Ok((text, meta)) => self.complete_workflow_node_with(&run_id, &node_id, text, meta),
            Err(err) => self.fail_workflow_node(&run_id, &node_id, err),
        }
        if let (Some(pane), Some(artifact)) = (viewer_pane, artifact) {
            self.display_workflow_image(pane, &run_id, &node_id, &artifact);
        }
        self.refresh_workflow_view();
    }

    /// Project a generated image into its viewer pane via kitty graphics
    /// (I2). Skips silently to the pane's path text when graphics are off,
    /// the artifact is not PNG, or the budget check fails.
    fn display_workflow_image(
        &mut self,
        pane: PaneId,
        run_id: &str,
        node_id: &str,
        artifact: &str,
    ) {
        if !self.state.kitty_graphics_enabled {
            return;
        }
        let path = runs::node_root(run_id, node_id).join(artifact);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(run_id, node_id, %err, "workflow image artifact unreadable");
                return;
            }
        };
        let Some((width, height)) = png_dimensions(&bytes) else {
            tracing::debug!(run_id, node_id, "workflow image artifact is not PNG");
            return;
        };
        let key = (pane, image_layer_id(node_id));
        if !self.pane_graphics.can_add_slot(&key)
            || !self.pane_graphics.can_store_inline(&key, bytes.len())
        {
            tracing::debug!(
                run_id,
                node_id,
                len = bytes.len(),
                "workflow image exceeds the pane graphics budget"
            );
            return;
        }
        let Some(host_image_id) = self.pane_graphics.reserve_image_id(&key) else {
            return;
        };
        let layer = crate::app::pane_graphics::Layer::inline(
            crate::api::schema::PaneGraphicsFormat::Png,
            width,
            height,
            bytes,
            crate::api::schema::PaneGraphicsPlacementParams::default(),
            0,
        );
        self.pane_graphics.slots.insert(
            key,
            crate::app::pane_graphics::Slot {
                host_image_id,
                layer: Some(layer),
                stream_owner: None,
                stream_active: None,
                direct_gate: None,
            },
        );
        self.pane_graphics.mark_changed();
    }

    fn complete_workflow_node(
        &mut self,
        run_id: &str,
        node_id: &str,
        output: String,
        meta: &NodeMeta,
    ) {
        self.complete_workflow_node_with(run_id, node_id, output, meta.clone());
    }

    fn complete_workflow_node_with(
        &mut self,
        run_id: &str,
        node_id: &str,
        output: String,
        meta: NodeMeta,
    ) {
        // Child output is untrusted at this persistence boundary: a config or
        // auth echo would otherwise land in output.txt verbatim.
        let output = self.redact_workflow_node_text(run_id, node_id, output);
        cleanup_runtime_homes(run_id, node_id);
        let Some(live) = self.workflow_runs.get_mut(run_id) else {
            return;
        };
        let (config_hash, inputs_hash) = workflow_cache_keys(&live.engine, node_id);
        let meta = NodeMeta {
            config_hash,
            inputs_hash,
            ..meta
        };
        let hash = match runs::save_node_result(run_id, node_id, &output, &meta) {
            Ok(hash) => hash,
            Err(err) => {
                tracing::warn!(run_id, node_id, err = %err, "failed to persist node result");
                crate::workflow::runs::output_hash(&output)
            }
        };
        let outcome = live.engine.mark_done(node_id, output, hash);
        if let Some(status) = outcome {
            self.finish_workflow_run(run_id, status);
        } else {
            self.persist_workflow_run(run_id);
            self.dispatch_workflow_ready_nodes();
        }
    }

    fn fail_workflow_node(&mut self, run_id: &str, node_id: &str, error: String) {
        let error = self.redact_workflow_node_text(run_id, node_id, error);
        cleanup_runtime_homes(run_id, node_id);
        let Some(live) = self.workflow_runs.get_mut(run_id) else {
            return;
        };
        live.engine.mark_error(node_id, error);
        self.finish_workflow_run(run_id, RunStatus::Error);
    }

    /// Redact a node's captured text/error against its bound profile key —
    /// grok/dsh config echoes are the realistic leak surface.
    fn redact_workflow_node_text(&self, run_id: &str, node_id: &str, text: String) -> String {
        let Some(profile_id) = self
            .workflow_runs
            .get(run_id)
            .and_then(|live| live.engine.def.node(node_id))
            .and_then(|node| node.provider_profile_id.clone())
        else {
            return text;
        };
        let Some(api_key) = crate::persist::provider_registry::load()
            .into_iter()
            .find(|profile| profile.id == profile_id)
            .map(|profile| profile.api_key)
        else {
            return text;
        };
        if api_key.is_empty() {
            text
        } else {
            crate::provider::url::redact(&api_key, &text)
        }
    }

    /// Terminal cleanup: persist the final record, drop live state, and
    /// prune old runs. Done/Error runs keep their workspace (final outputs
    /// and image viewer panes stay inspectable; W10 cleanup happens on
    /// cancel/delete/prune); cancel tears the workspace down immediately.
    fn finish_workflow_run(&mut self, run_id: &str, status: RunStatus) {
        let Some(mut live) = self.workflow_runs.remove(run_id) else {
            return;
        };
        live.engine.status = status;
        let record = live.engine.record(live.started_unix, Some(now_unix()));
        let workspace_idx = live.workspace_idx;
        let _ = runs::save_record(&record);
        // Cancel never passes through complete/fail: sweep leftover temp
        // homes for every node of this run.
        for node in &live.engine.def.nodes {
            cleanup_runtime_homes(run_id, &node.id);
        }

        if status == RunStatus::Cancelled {
            self.close_workflow_workspace(&record.workspace_id, workspace_idx);
        }

        let keep = self.workflow_limits.keep_runs;
        for (_, workspace_id) in runs::prune_runs(keep) {
            // The run directory is gone; its kept-open workspace goes too.
            self.close_workflow_workspace(&workspace_id, usize::MAX);
        }
        self.schedule_session_save();
    }

    /// Close a run workspace by id (falls back to the live index when the
    /// id no longer resolves).
    fn close_workflow_workspace(&mut self, workspace_id: &Option<String>, fallback_idx: usize) {
        let workspace_idx = workspace_id
            .as_ref()
            .and_then(|id| self.state.workspaces.iter().position(|ws| ws.id == *id))
            .unwrap_or(fallback_idx);
        if self.state.workspaces.get(workspace_idx).is_some() {
            let terminal_ids = self.state.terminal_ids_for_workspace(workspace_idx);
            let pane_ids = self.state.pane_ids_for_workspace(workspace_idx);
            self.state.remove_plugin_pane_records(pane_ids);
            if self.state.workspaces.len() > workspace_idx {
                self.state.workspaces.remove(workspace_idx);
            }
            self.state.remove_unattached_terminal_ids(terminal_ids);
            self.shutdown_detached_terminal_runtimes();
        }
    }

    fn persist_workflow_run(&mut self, run_id: &str) {
        let Some(live) = self.workflow_runs.get(run_id) else {
            return;
        };
        let record: RunRecord = live.engine.record(live.started_unix, None);
        let _ = runs::save_record(&record);
    }

    /// Fill `outputs`/`Done` phases from the on-disk cache (resume path).
    fn apply_workflow_cache(&mut self, run_id: &str) {
        let node_ids: Vec<String> = {
            let Some(live) = self.workflow_runs.get(run_id) else {
                return;
            };
            live.engine
                .def
                .nodes
                .iter()
                .map(|node| node.id.clone())
                .collect()
        };
        for node_id in node_ids {
            let (config_hash, inputs_hash) = {
                let Some(live) = self.workflow_runs.get(run_id) else {
                    return;
                };
                workflow_cache_keys(&live.engine, &node_id)
            };
            if let Some((output, meta)) =
                runs::load_cached_node(run_id, &node_id, &config_hash, &inputs_hash)
            {
                let hash = crate::workflow::runs::output_hash(&output);
                if let Some(live) = self.workflow_runs.get_mut(run_id) {
                    live.engine.mark_cached(&node_id, output, hash);
                    let _ = meta;
                }
            }
        }
    }

    // -- timeouts -------------------------------------------------------------

    /// Called from the scheduled-tasks path (both loop variants); kills
    /// nodes whose deadline passed.
    pub(crate) fn tick_workflow_timeouts(&mut self, now: Instant) -> bool {
        if self.workflow_runs.is_empty() {
            return false;
        }
        let mut expired: Vec<(String, String)> = Vec::new();
        for (run_id, live) in &self.workflow_runs {
            for (node_id, deadline) in &live.deadline_of_node {
                if now >= *deadline {
                    expired.push((run_id.clone(), node_id.clone()));
                }
            }
        }
        if expired.is_empty() {
            return false;
        }
        for (run_id, node_id) in expired {
            // Kill the visible pane / invisible process, then fail the node.
            if let Some(live) = self.workflow_runs.get(&run_id) {
                if let Some(pane_id) = live.pane_of_node.get(&node_id).copied() {
                    let terminal_id = self
                        .state
                        .workspaces
                        .get(live.workspace_idx)
                        .and_then(|ws| ws.terminal_id(pane_id))
                        .cloned();
                    if let Some(terminal_id) = terminal_id {
                        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
                            runtime.shutdown();
                        }
                    }
                }
                if let Some(pid_slot) = live.pid_of_node.get(&node_id) {
                    if let Some(pid) = pid_slot.lock().expect("pid lock").take() {
                        let pids = crate::platform::session_processes(pid);
                        crate::platform::signal_processes(
                            &pids,
                            crate::platform::Signal::Terminate,
                        );
                    }
                }
            }
            self.fail_workflow_node(&run_id, &node_id, "node timed out".to_string());
        }
        self.refresh_workflow_view();
        true
    }

    /// Live status snapshot for API responses.
    pub(crate) fn workflow_live_record(&self, run_id: &str) -> Option<RunRecord> {
        let live = self.workflow_runs.get(run_id)?;
        Some(live.engine.record(live.started_unix, None))
    }

    // -- graph view projection -------------------------------------------------

    /// Rebuild the AppState workflow projection: the sidebar runs list
    /// (disk records overlaid with live state, same merge as
    /// `workflow.list`) and, when a graph is open, its snapshot. Called once
    /// per workflow event — never from render.
    pub(crate) fn refresh_workflow_view(&mut self) {
        let mut records = runs::load_all_records();
        let live_ids: Vec<String> = self.workflow_runs.keys().cloned().collect();
        for run_id in live_ids {
            if let Some(live) = self.workflow_live_record(&run_id) {
                match records.iter_mut().find(|record| record.run_id == run_id) {
                    Some(record) => *record = live,
                    None => records.insert(0, live),
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.started_unix));
        self.state.workflow_view.runs = records
            .iter()
            .map(|record| WorkflowRunSummary {
                run_id: record.run_id.clone(),
                workflow_name: record.workflow_name.clone(),
                status: record.status.as_str().to_string(),
                started_unix: record.started_unix,
                done_count: record
                    .nodes
                    .iter()
                    .filter(|node| node.phase == NodePhase::Done)
                    .count(),
                total_nodes: record.nodes.len(),
            })
            .collect();

        let Some(open_id) = self
            .state
            .workflow_view
            .open
            .as_ref()
            .map(|snapshot| snapshot.run_id.clone())
        else {
            return;
        };
        match self.build_workflow_graph_snapshot(&open_id) {
            Some(snapshot) => self.state.workflow_view.open = Some(snapshot),
            None => self.state.workflow_view.open = None,
        }
    }

    /// Open the graph view for one run (live or historical). Returns false
    /// when the run no longer exists.
    pub(crate) fn open_workflow_graph(&mut self, run_id: &str) -> bool {
        let Some(snapshot) = self.build_workflow_graph_snapshot(run_id) else {
            return false;
        };
        let view = &mut self.state.workflow_view;
        view.selection = 0;
        view.scroll_x = 0;
        view.scroll_y = 0;
        view.inspector = None;
        view.confirm_cancel = false;
        view.open = Some(snapshot);
        self.state.mode = Mode::WorkflowGraph;
        true
    }

    fn build_workflow_graph_snapshot(&self, run_id: &str) -> Option<WorkflowGraphSnapshot> {
        if let Some(live) = self.workflow_runs.get(run_id) {
            let engine = &live.engine;
            let pane_details = self
                .state
                .workspaces
                .get(live.workspace_idx)
                .map(|ws| ws.pane_details(&self.state.terminals));
            let nodes = engine
                .def
                .nodes
                .iter()
                .map(|node| {
                    let state = engine.nodes.get(&node.id);
                    let pane = live
                        .pane_of_node
                        .get(&node.id)
                        .or_else(|| live.viewer_pane_of_node.get(&node.id))
                        .copied();
                    let agent_state = pane.and_then(|pane_id| {
                        pane_details.as_ref().and_then(|details| {
                            details
                                .iter()
                                .find(|detail| detail.pane_id == pane_id)
                                .map(|detail| detail.state)
                        })
                    });
                    let meta = runs::load_node_meta(run_id, &node.id);
                    WorkflowNodeView {
                        id: node.id.clone(),
                        title: node.display_title().to_string(),
                        kind: node_kind_str(node.node_type).to_string(),
                        runtime: node.runtime.map(|runtime| runtime.label().to_string()),
                        profile_id: node.provider_profile_id.clone(),
                        model: node
                            .model
                            .clone()
                            .or_else(|| meta.as_ref().and_then(|meta| meta.model.clone())),
                        visible: node.visible,
                        enabled: node.enabled,
                        timeout_ms: node.timeout_ms,
                        phase: state
                            .map(|state| node_phase_str(state.phase).to_string())
                            .unwrap_or_else(|| "pending".to_string()),
                        cached: state.is_some_and(|state| state.cached),
                        error: state.and_then(|state| state.error.clone()),
                        cost_usd: meta.as_ref().and_then(|meta| meta.cost_usd),
                        tokens: meta.as_ref().and_then(|meta| meta.tokens),
                        artifact: meta.as_ref().and_then(|meta| meta.artifact.clone()),
                        pane,
                        agent_state,
                        sort_y: node.position.map(|position| position.y),
                    }
                })
                .collect();
            return Some(WorkflowGraphSnapshot {
                run_id: run_id.to_string(),
                workflow_name: engine.def.name.clone(),
                path: engine.workflow_path.clone(),
                status: engine.status.as_str().to_string(),
                live: true,
                workspace_idx: Some(live.workspace_idx),
                nodes,
                edges: graph::edges(&engine.def),
            });
        }
        // Historical run: disk record + the (possibly edited) workflow file.
        let record = runs::load_record(run_id)?;
        let text = std::fs::read_to_string(&record.workflow_path).ok()?;
        let def = WorkflowDef::parse(&text).ok()?;
        let nodes = def
            .nodes
            .iter()
            .map(|node| {
                let node_record = record.nodes.iter().find(|n| n.id == node.id);
                let meta = runs::load_node_meta(run_id, &node.id);
                WorkflowNodeView {
                    id: node.id.clone(),
                    title: node.display_title().to_string(),
                    kind: node_kind_str(node.node_type).to_string(),
                    runtime: node.runtime.map(|runtime| runtime.label().to_string()),
                    profile_id: node.provider_profile_id.clone(),
                    model: node
                        .model
                        .clone()
                        .or(meta.as_ref().and_then(|m| m.model.clone())),
                    visible: node.visible,
                    enabled: node.enabled,
                    timeout_ms: node.timeout_ms,
                    phase: node_record
                        .map(|n| node_phase_str(n.phase).to_string())
                        .unwrap_or_else(|| "pending".to_string()),
                    cached: node_record.is_some_and(|n| n.cached),
                    error: node_record.and_then(|n| n.error.clone()),
                    cost_usd: meta.as_ref().and_then(|meta| meta.cost_usd),
                    tokens: meta.as_ref().and_then(|meta| meta.tokens),
                    artifact: meta.as_ref().and_then(|meta| meta.artifact.clone()),
                    pane: None,
                    agent_state: None,
                    sort_y: node.position.map(|position| position.y),
                }
            })
            .collect();
        Some(WorkflowGraphSnapshot {
            run_id: run_id.to_string(),
            workflow_name: record.workflow_name.clone(),
            path: record.workflow_path.clone(),
            status: record.status.as_str().to_string(),
            live: false,
            workspace_idx: record
                .workspace_id
                .and_then(|id| self.state.workspaces.iter().position(|ws| ws.id == id)),
            nodes,
            edges: graph::edges(&def),
        })
    }
}

fn node_kind_str(node_type: NodeType) -> &'static str {
    match node_type {
        NodeType::Agent => "agent",
        NodeType::PromptTemplate => "prompt_template",
        NodeType::ImageGen => "image_gen",
    }
}

/// Failed nodes re-run on resume (W9 fix-and-rerun).
fn reset_errored_nodes(engine: &mut EngineRun) {
    let errored: Vec<String> = engine
        .nodes
        .iter()
        .filter(|(_, node)| node.phase == NodePhase::Error)
        .map(|(id, _)| id.clone())
        .collect();
    for node_id in errored {
        if let Some(node) = engine.nodes.get_mut(&node_id) {
            node.phase = NodePhase::Pending;
            node.output_hash = None;
            node.error = None;
            node.cached = false;
        }
    }
}

/// PNG magic + IHDR dimensions (real pixel size, not the requested size —
/// the renderer crops from these).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

/// Pane-graphics layer id for a workflow node (layer ids are restricted to
/// `[A-Za-z0-9._:-]`, max 64 chars).
fn image_layer_id(node_id: &str) -> String {
    let sanitized: String = node_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-') {
                ch
            } else {
                '-'
            }
        })
        .take(60)
        .collect();
    format!("wf-{sanitized}")
}

fn workflow_cache_keys(engine: &EngineRun, node_id: &str) -> (String, String) {
    let config_hash =
        crate::workflow::runs::node_config_hash(&engine.def, node_id).unwrap_or_default();
    let output_hashes: HashMap<String, String> = engine
        .nodes
        .iter()
        .filter_map(|(id, node)| node.output_hash.clone().map(|hash| (id.clone(), hash)))
        .collect();
    let inputs_hash = crate::workflow::runs::node_inputs_hash(&engine.def, node_id, &output_hashes)
        .unwrap_or_default();
    (config_hash, inputs_hash)
}

/// Remove the per-node temp homes (grok/dsh injection artifacts; FR-9.2).
/// Cancel skips complete/fail, so `finish_workflow_run` sweeps leftovers.
fn cleanup_runtime_homes(run_id: &str, node_id: &str) {
    for home in [".af-grok-home", ".af-dsh-home"] {
        let _ = std::fs::remove_dir_all(runs::node_root(run_id, node_id).join(home));
    }
}

fn shell_string_command(string: &str) -> std::process::Command {
    if cfg!(windows) {
        let mut command = crate::noninteractive_process::command("cmd");
        command.arg("/d").arg("/c").arg(string);
        command
    } else {
        let mut command = crate::noninteractive_process::command("/bin/sh");
        command.arg("-c").arg(string);
        command
    }
}

fn read_capped<R: std::io::Read>(mut pipe: Option<R>, cap: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        let mut limited = pipe.take(cap as u64);
        let _ = limited.read_to_end(&mut bytes);
    }
    bytes
}

/// Node-phase string used by API snapshots.
pub(crate) fn node_phase_str(phase: NodePhase) -> &'static str {
    match phase {
        NodePhase::Pending => "pending",
        NodePhase::Running => "running",
        NodePhase::Done => "done",
        NodePhase::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dimensions_reads_ihdr() {
        let mut bytes = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        bytes.extend_from_slice(&1024u32.to_be_bytes());
        bytes.extend_from_slice(&768u32.to_be_bytes());
        assert_eq!(png_dimensions(&bytes), Some((1024, 768)));
        assert_eq!(png_dimensions(b"not a png at all........"), None);
        assert_eq!(png_dimensions(&bytes[..20]), None);
    }

    #[test]
    fn image_layer_id_uses_layer_charset() {
        assert_eq!(image_layer_id("gen icon"), "wf-gen-icon");
        let id = image_layer_id(&"x".repeat(200));
        assert!(id.len() <= 64);
        assert!(id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-')));
    }
}
