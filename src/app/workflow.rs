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

use crate::events::{AppEvent, WorkflowNodeFinished};
use crate::layout::PaneId;
use crate::workflow::engine::EngineRun;
use crate::workflow::executors::agent::{
    build_agent_command, expand_env_placeholders, parse_node_output, provider_env,
    sanitized_profile_key, shell_wrap_visible, AgentCommand,
};
use crate::workflow::executors::image::run_image_gen;
use crate::workflow::executors::template::render;
use crate::workflow::model::{NodeType, WorkflowDef, WorkflowNode};
use crate::workflow::runs::{self, NodeMeta, NodePhase, RunRecord, RunStatus};

/// Live per-run execution state (engine + pane/process bindings).
pub(crate) struct WorkflowRunLive {
    pub engine: EngineRun,
    pub workspace_idx: usize,
    pub started_unix: u64,
    pub pane_of_node: HashMap<String, PaneId>,
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
        let text = std::fs::read_to_string(path)
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
                pid_of_node: HashMap::new(),
                deadline_of_node: HashMap::new(),
            },
        );
        self.label_workflow_workspace(workspace_idx, &run_id);
        self.dispatch_workflow_ready_nodes();
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
        Ok(())
    }

    pub(crate) fn resume_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        let Some(record) = runs::load_record(run_id) else {
            return Err(format!("run {run_id} not found"));
        };
        if record.status != RunStatus::Paused {
            return Err(format!(
                "run {run_id} is {} ; only paused runs resume",
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

        let started_unix = record.started_unix;
        self.workflow_runs.insert(
            run_id.to_string(),
            WorkflowRunLive {
                engine,
                workspace_idx,
                started_unix,
                pane_of_node: HashMap::new(),
                pid_of_node: HashMap::new(),
                deadline_of_node: HashMap::new(),
            },
        );
        self.apply_workflow_cache(run_id);
        self.persist_workflow_run(run_id);
        self.dispatch_workflow_ready_nodes();
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
        Ok(())
    }

    pub(crate) fn delete_workflow_run(&mut self, run_id: &str) -> Result<(), String> {
        if self.workflow_runs.contains_key(run_id) {
            self.cancel_workflow_run(run_id)?;
        }
        runs::delete_run(run_id).map_err(|err| err.to_string())?;
        Ok(())
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
        let command =
            match build_agent_command(node, &prompt, profile.as_ref(), sanitized_key.as_deref()) {
                Ok(command) => command,
                Err(err) => {
                    self.fail_workflow_node(run_id, &node.id, err);
                    return false;
                }
            };
        let env = profile
            .as_ref()
            .map(|profile| provider_env(profile, sanitized_key.as_deref(), node.model.as_deref()))
            .unwrap_or_default();
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

        // The shell wrapper tees into the node directory; it must exist
        // before the pane/process starts, or Tee-Object fails silently.
        let _ = std::fs::create_dir_all(runs::node_root(run_id, &node.id));

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
                Err(format!(
                    "process exited with code {}",
                    effective_exit.unwrap_or(-1)
                ))
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
            self.fail_workflow_node(
                &run_id,
                &node_id,
                format!(
                    "process exited with code {}",
                    output.exit_code.unwrap_or(-1)
                ),
            );
        }
        true
    }

    pub(crate) fn handle_workflow_node_finished(&mut self, event: Box<WorkflowNodeFinished>) {
        let WorkflowNodeFinished {
            run_id,
            node_id,
            result,
        } = *event;
        if let Some(live) = self.workflow_runs.get_mut(&run_id) {
            live.pid_of_node.remove(&node_id);
            live.deadline_of_node.remove(&node_id);
        }
        match result {
            Ok((text, meta)) => self.complete_workflow_node_with(&run_id, &node_id, text, meta),
            Err(err) => self.fail_workflow_node(&run_id, &node_id, err),
        }
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
        let Some(live) = self.workflow_runs.get_mut(run_id) else {
            return;
        };
        live.engine.mark_error(node_id, error);
        self.finish_workflow_run(run_id, RunStatus::Error);
    }

    /// Terminal cleanup: persist the final record, drop live state, close
    /// the run workspace, and prune old runs.
    fn finish_workflow_run(&mut self, run_id: &str, status: RunStatus) {
        let Some(mut live) = self.workflow_runs.remove(run_id) else {
            return;
        };
        live.engine.status = status;
        let record = live.engine.record(live.started_unix, Some(now_unix()));
        let workspace_idx = live.workspace_idx;
        let _ = runs::save_record(&record);

        // Close the run workspace (panes may already be gone; the console
        // tab keeps the workspace alive until now).
        let workspace_id = record.workspace_id.clone();
        let workspace_idx = self
            .state
            .workspaces
            .iter()
            .position(|ws| Some(&ws.id) == workspace_id.as_ref())
            .unwrap_or(workspace_idx);
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

        let keep = self.workflow_limits.keep_runs;
        let _ = runs::prune_runs(keep);
        self.schedule_session_save();
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
        true
    }

    /// Live status snapshot for API responses.
    pub(crate) fn workflow_live_record(&self, run_id: &str) -> Option<RunRecord> {
        let live = self.workflow_runs.get(run_id)?;
        Some(live.engine.record(live.started_unix, None))
    }
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
