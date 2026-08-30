//! Workflow graph view input: node navigation, pane jump, the node
//! inspector, and run control. Pure state fn + returned actions (the
//! Providers settings section is the pattern: precedence-routed layers,
//! single-field edit sessions, layer-local Esc).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::schema::{
    Method, WorkflowCancelParams, WorkflowNodePatch, WorkflowPauseParams, WorkflowResumeParams,
    WorkflowUpdateParams,
};
use crate::app::state::{
    AppState, SelectionListState, WorkflowInspectorChoice, WorkflowInspectorEdit,
    WorkflowInspectorField, WorkflowInspectorState, WorkflowNodeView,
};
use crate::layout::PaneId;
use crate::workflow::graph::{self, GraphLayout, CARD_HEIGHT, ROW_PITCH};

#[derive(Debug)]
pub(crate) enum WorkflowGraphAction {
    FocusNodePane {
        workspace_idx: usize,
        pane_id: PaneId,
    },
    /// The inspector was requested; the App primes it (masked profiles come
    /// from the on-disk registry, which this pure layer cannot read).
    OpenInspector { node_id: String },
    /// Jump to another run's graph (`<` / `>` cycle the runs list).
    SwitchRun { run_id: String },
    /// Shared JSON-API path (pause/resume/cancel/update) so TUI and CLI hit
    /// identical handlers.
    Api(Box<Method>),
}

impl crate::app::App {
    /// Entry for both key-dispatch tables (monolithic + headless).
    pub(crate) fn handle_workflow_graph_key(&mut self, key: KeyEvent) {
        if let Some(action) = update_workflow_graph_state(&mut self.state, key) {
            match action {
                WorkflowGraphAction::FocusNodePane {
                    workspace_idx,
                    pane_id,
                } => {
                    self.state.switch_workspace(workspace_idx);
                    self.state.focus_pane_in_workspace(workspace_idx, pane_id);
                    self.state.mode = crate::app::state::Mode::Terminal;
                }
                WorkflowGraphAction::OpenInspector { node_id } => {
                    let profiles = self.masked_provider_profiles();
                    let pools = self.workflow_pool_names();
                    open_inspector(&mut self.state, &node_id, profiles, pools);
                }
                WorkflowGraphAction::SwitchRun { run_id } => {
                    self.open_workflow_graph(&run_id);
                }
                WorkflowGraphAction::Api(method) => {
                    let _ = self.dispatch_runtime_mutation("tui.workflow.graph", *method);
                    self.refresh_workflow_view();
                }
            }
        }
    }
}

/// Field order of the inspector list.
const INSPECTOR_FIELDS: [WorkflowInspectorField; 7] = [
    WorkflowInspectorField::Runtime,
    WorkflowInspectorField::Profile,
    WorkflowInspectorField::Pool,
    WorkflowInspectorField::Model,
    WorkflowInspectorField::TimeoutMs,
    WorkflowInspectorField::Visible,
    WorkflowInspectorField::Enabled,
];

const RUNTIME_LABELS: [&str; 5] = ["claude-code", "codex", "grok-build", "dsh", "custom"];

pub(crate) fn update_workflow_graph_state(
    state: &mut AppState,
    key: KeyEvent,
) -> Option<WorkflowGraphAction> {
    state.workflow_view.notice = None;
    let editing = state
        .workflow_view
        .inspector
        .as_ref()
        .is_some_and(|inspector| inspector.edit.is_some());
    let choosing = state
        .workflow_view
        .inspector
        .as_ref()
        .is_some_and(|inspector| inspector.choice.is_some());
    if state.workflow_view.inspector.is_some() {
        return if editing {
            inspector_edit_key(state, key)
        } else if choosing {
            inspector_choice_key(state, key)
        } else {
            inspector_list_key(state, key)
        };
    }
    graph_key(state, key)
}

fn plain(key: &KeyEvent) -> bool {
    key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
}

fn graph_key(state: &mut AppState, key: KeyEvent) -> Option<WorkflowGraphAction> {
    match key.code {
        KeyCode::Esc => {
            super::modal::leave_modal(state);
            None
        }
        KeyCode::Left | KeyCode::Char('h') if plain(&key) => {
            move_selection(state, -1, 0);
            None
        }
        KeyCode::Right | KeyCode::Char('l') if plain(&key) => {
            move_selection(state, 1, 0);
            None
        }
        KeyCode::Up | KeyCode::Char('k') if plain(&key) => {
            move_selection(state, 0, -1);
            None
        }
        KeyCode::Down | KeyCode::Char('j') if plain(&key) => {
            move_selection(state, 0, 1);
            None
        }
        KeyCode::Enter => enter_action(state),
        // Tab / Shift+Tab are the primary run-cycle keys: control keys are
        // immune to IME fullwidth punctuation ('<' / '>' aliases break under
        // Chinese input methods and stay as a fallback).
        KeyCode::Tab if key.modifiers.is_empty() => cycle_run(state, 1),
        KeyCode::Tab if key.modifiers == KeyModifiers::SHIFT => cycle_run(state, -1),
        KeyCode::Char('>') if plain(&key) => cycle_run(state, 1),
        KeyCode::Char('<') if plain(&key) => cycle_run(state, -1),
        KeyCode::Char('i') if plain(&key) => {
            let node_id = selected_node(&state.workflow_view)?.id.clone();
            Some(WorkflowGraphAction::OpenInspector { node_id })
        }
        KeyCode::Char('p') if plain(&key) => toggle_pause(state),
        KeyCode::Char('x') if plain(&key) => {
            if state.workflow_view.confirm_cancel {
                state.workflow_view.confirm_cancel = false;
                let run_id = state
                    .workflow_view
                    .open
                    .as_ref()
                    .map(|snapshot| snapshot.run_id.clone())?;
                Some(WorkflowGraphAction::Api(Box::new(Method::WorkflowCancel(
                    WorkflowCancelParams { run_id },
                ))))
            } else {
                state.workflow_view.confirm_cancel = true;
                state.workflow_view.notice =
                    Some("press x again to cancel this run (esc disarms)".to_string());
                None
            }
        }
        _ => None,
    }
}

fn selected_node(view: &crate::app::state::WorkflowViewState) -> Option<&WorkflowNodeView> {
    let snapshot = view.open.as_ref()?;
    snapshot.nodes.get(view.selection)
}

/// Move the selection to the nearest card center in the given direction
/// (dx/dy ∈ {-1, 0, 1}, not both zero), then keep the card scrolled in.
fn move_selection(state: &mut AppState, dx: isize, dy: isize) {
    let (layout, node_ids): (GraphLayout, Vec<String>) = {
        let Some(snapshot) = state.workflow_view.open.as_ref() else {
            return;
        };
        if snapshot.nodes.is_empty() {
            return;
        }
        (
            snapshot.graph_layout(),
            snapshot.nodes.iter().map(|node| node.id.clone()).collect(),
        )
    };
    let current_id = selected_node(&state.workflow_view)
        .map(|node| node.id.clone())
        .unwrap_or_default();
    let Some(current_card) = layout.card(&current_id) else {
        state.workflow_view.selection = 0;
        return;
    };
    let center = |card: &graph::GraphNodeCard| {
        (
            layout.col_x[card.col] as isize + card.width as isize / 2,
            card.row as isize * ROW_PITCH as isize + CARD_HEIGHT as isize / 2,
        )
    };
    let (cx, cy) = center(current_card);
    let mut best: Option<(isize, usize)> = None;
    for (position, node_id) in node_ids.iter().enumerate() {
        let Some(card) = layout.card(node_id) else {
            continue;
        };
        if card.id == current_card.id {
            continue;
        }
        let (nx, ny) = center(card);
        let (ddx, ddy) = (nx - cx, ny - cy);
        let in_direction = match (dx, dy) {
            (1, 0) => ddx > 0,
            (-1, 0) => ddx < 0,
            (0, 1) => ddy > 0,
            (0, -1) => ddy < 0,
            _ => false,
        };
        if !in_direction {
            continue;
        }
        let score = ddx.abs() + ddy.abs();
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, position));
        }
    }
    if let Some((_, index)) = best {
        state.workflow_view.selection = index;
        clamp_scroll_to_selection(state, &layout);
    }
}

/// Adjust scroll offsets so the selected card lies fully inside the body.
fn clamp_scroll_to_selection(state: &mut AppState, layout: &GraphLayout) {
    let body = state.workflow_graph_body_rect();
    let (card_x, card_y, card_w) = {
        let Some(snapshot) = state.workflow_view.open.as_ref() else {
            return;
        };
        let Some(node) = snapshot.nodes.get(state.workflow_view.selection) else {
            return;
        };
        let Some(card) = layout.card(&node.id) else {
            return;
        };
        (
            layout.col_x[card.col] as u16,
            (card.row * ROW_PITCH) as u16,
            card.width as u16,
        )
    };
    let (scroll_x, scroll_y) = (
        state.workflow_view.scroll_x as u16,
        state.workflow_view.scroll_y as u16,
    );
    if card_x < scroll_x {
        state.workflow_view.scroll_x = card_x as usize;
    } else if body.width > 0 && card_x + card_w > scroll_x + body.width {
        state.workflow_view.scroll_x = (card_x + card_w - body.width) as usize;
    }
    if card_y < scroll_y {
        state.workflow_view.scroll_y = card_y as usize;
    } else if body.height > 0 && card_y + CARD_HEIGHT as u16 > scroll_y + body.height {
        state.workflow_view.scroll_y = (card_y + CARD_HEIGHT as u16 - body.height) as usize;
    }
}

fn enter_action(state: &mut AppState) -> Option<WorkflowGraphAction> {
    let (pane, workspace_idx, phase, visible, live, run_id, artifact) = {
        let snapshot = state.workflow_view.open.as_ref()?;
        let node = snapshot.nodes.get(state.workflow_view.selection)?;
        (
            node.pane,
            snapshot.workspace_idx,
            node.phase.clone(),
            node.visible,
            snapshot.live,
            snapshot.run_id.clone(),
            node.artifact.clone(),
        )
    };
    if let (Some(pane_id), Some(workspace_idx)) = (pane, workspace_idx) {
        return Some(WorkflowGraphAction::FocusNodePane {
            workspace_idx,
            pane_id,
        });
    }
    // No live pane: point at the on-disk artifacts.
    let run_dir = crate::workflow::runs::run_root(&run_id);
    let detail = if !live {
        "run finished"
    } else if phase == "running" && !visible {
        "background node"
    } else {
        "pane closed"
    };
    let artifact_note = artifact
        .as_deref()
        .map(|artifact| format!(" ({artifact})"))
        .unwrap_or_default();
    state.workflow_view.notice = Some(format!(
        "{detail}; output & logs: {}{artifact_note}",
        run_dir.display()
    ));
    None
}

/// Cycle to the previous/next openable run in the sidebar list (wraps;
/// historical runs whose workflow file vanished are skipped).
fn cycle_run(state: &mut AppState, direction: isize) -> Option<WorkflowGraphAction> {
    let view = &state.workflow_view;
    let runs = &view.runs;
    if runs.len() < 2 {
        return None;
    }
    let current = view.open.as_ref().map(|snapshot| &snapshot.run_id)?;
    let index = runs.iter().position(|run| &run.run_id == current)?;
    for step in 1..runs.len() {
        let candidate =
            (index as isize + direction * step as isize).rem_euclid(runs.len() as isize) as usize;
        if runs[candidate].path_valid {
            let run_id = runs[candidate].run_id.clone();
            return Some(WorkflowGraphAction::SwitchRun { run_id });
        }
    }
    state.workflow_view.notice = Some("no other runs with an existing workflow file".to_string());
    None
}

fn toggle_pause(state: &mut AppState) -> Option<WorkflowGraphAction> {
    let (status, run_id) = {
        let snapshot = state.workflow_view.open.as_ref()?;
        (snapshot.status.clone(), snapshot.run_id.clone())
    };
    match status.as_str() {
        "running" => Some(WorkflowGraphAction::Api(Box::new(Method::WorkflowPause(
            WorkflowPauseParams { run_id },
        )))),
        "paused" | "error" | "partial_fail" => Some(WorkflowGraphAction::Api(Box::new(
            Method::WorkflowResume(WorkflowResumeParams { run_id }),
        ))),
        other => {
            state.workflow_view.notice = Some(format!("run is {other}; nothing to toggle"));
            None
        }
    }
}

// -- inspector ---------------------------------------------------------------

fn inspector_list_key(state: &mut AppState, key: KeyEvent) -> Option<WorkflowGraphAction> {
    let field_count = INSPECTOR_FIELDS.len();
    match key.code {
        KeyCode::Esc => {
            state.workflow_view.inspector = None;
            None
        }
        KeyCode::Up | KeyCode::Char('k') if plain(&key) => {
            state.workflow_view.inspector.as_mut()?.list.move_prev();
            None
        }
        KeyCode::Down | KeyCode::Char('j') if plain(&key) => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .list
                .move_next(field_count);
            None
        }
        KeyCode::Enter => {
            let selected = state.workflow_view.inspector.as_ref()?.list.selected;
            let field = INSPECTOR_FIELDS[selected.min(field_count - 1)];
            open_field_editor(state, field)
        }
        _ => None,
    }
}

fn open_field_editor(
    state: &mut AppState,
    field: WorkflowInspectorField,
) -> Option<WorkflowGraphAction> {
    let node: WorkflowNodeView = {
        let view = &state.workflow_view;
        let snapshot = view.open.as_ref()?;
        let node_id = view.inspector.as_ref()?.node_id.clone();
        snapshot.node(&node_id)?.clone()
    };
    let profiles: Vec<(String, String)> = state
        .workflow_view
        .inspector
        .as_ref()?
        .profiles
        .iter()
        .map(|profile| (profile.id.clone(), profile.name.clone()))
        .collect();
    let pools: Vec<String> = state.workflow_view.inspector.as_ref()?.pools.clone();
    let inspector = state.workflow_view.inspector.as_mut()?;
    match field {
        WorkflowInspectorField::Runtime => {
            inspector.choice = Some(WorkflowInspectorChoice {
                field,
                list: SelectionListState::new(
                    node.runtime
                        .as_deref()
                        .and_then(|current| {
                            RUNTIME_LABELS.iter().position(|label| *label == current)
                        })
                        .unwrap_or(0),
                ),
                options: RUNTIME_LABELS
                    .iter()
                    .map(|label| label.to_string())
                    .collect(),
            });
        }
        WorkflowInspectorField::Profile => {
            let mut options = vec!["(unbound)".to_string()];
            options.extend(profiles.iter().map(|(_, name)| name.clone()));
            let selected = node
                .profile_id
                .as_deref()
                .and_then(|id| profiles.iter().position(|(profile_id, _)| profile_id == id))
                .map_or(0, |index| index + 1);
            inspector.choice = Some(WorkflowInspectorChoice {
                field,
                list: SelectionListState::new(selected),
                options,
            });
        }
        WorkflowInspectorField::Pool => {
            let mut options = vec!["(unbound)".to_string()];
            options.extend(pools.iter().cloned());
            let selected = node
                .provider_pool
                .as_deref()
                .and_then(|current| pools.iter().position(|pool| pool == current))
                .map_or(0, |index| index + 1);
            inspector.choice = Some(WorkflowInspectorChoice {
                field,
                list: SelectionListState::new(selected),
                options,
            });
        }
        WorkflowInspectorField::Visible | WorkflowInspectorField::Enabled => {
            let current = if field == WorkflowInspectorField::Visible {
                node.visible
            } else {
                node.enabled
            };
            inspector.choice = Some(WorkflowInspectorChoice {
                field,
                list: SelectionListState::new(if current { 0 } else { 1 }),
                options: vec!["true".to_string(), "false".to_string()],
            });
        }
        WorkflowInspectorField::Model => {
            inspector.edit = Some(WorkflowInspectorEdit {
                field,
                buffer: node.model.clone().unwrap_or_default(),
            });
        }
        WorkflowInspectorField::TimeoutMs => {
            inspector.edit = Some(WorkflowInspectorEdit {
                field,
                buffer: node.timeout_ms.to_string(),
            });
        }
    }
    None
}

fn inspector_choice_key(state: &mut AppState, key: KeyEvent) -> Option<WorkflowGraphAction> {
    let option_count = state
        .workflow_view
        .inspector
        .as_ref()?
        .choice
        .as_ref()?
        .options
        .len();
    match key.code {
        KeyCode::Esc => {
            state.workflow_view.inspector.as_mut()?.choice = None;
            None
        }
        KeyCode::Up | KeyCode::Char('k') if plain(&key) => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .choice
                .as_mut()?
                .list
                .move_prev();
            None
        }
        KeyCode::Down | KeyCode::Char('j') if plain(&key) => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .choice
                .as_mut()?
                .list
                .move_next(option_count);
            None
        }
        KeyCode::Enter => {
            let (field, index, options, profiles) = {
                let inspector = state.workflow_view.inspector.as_ref()?;
                let choice = inspector.choice.as_ref()?;
                (
                    choice.field,
                    choice.list.selected.min(option_count.saturating_sub(1)),
                    choice.options.clone(),
                    inspector.profiles.clone(),
                )
            };
            let raw = match field {
                // Map the profile picker row back to a profile id ("" clears).
                WorkflowInspectorField::Profile => {
                    if index == 0 {
                        String::new()
                    } else {
                        profiles
                            .get(index - 1)
                            .map(|profile| profile.id.clone())
                            .unwrap_or_default()
                    }
                }
                // Pool rows map back to group names the same way.
                WorkflowInspectorField::Pool => {
                    if index == 0 {
                        String::new()
                    } else {
                        options.get(index).cloned().unwrap_or_default()
                    }
                }
                _ => options[index].clone(),
            };
            state.workflow_view.inspector = None;
            apply_patch(state, field, &raw)
        }
        _ => None,
    }
}

fn inspector_edit_key(state: &mut AppState, key: KeyEvent) -> Option<WorkflowGraphAction> {
    // Extract what Enter needs first: the inspector borrow must end before
    // the action path mutates state.
    match key.code {
        KeyCode::Esc => {
            state.workflow_view.inspector.as_mut()?.edit = None;
            None
        }
        KeyCode::Enter => {
            let (field, buffer) = {
                let inspector = state.workflow_view.inspector.as_ref()?;
                let edit = inspector.edit.as_ref()?;
                (edit.field, edit.buffer.clone())
            };
            if field == WorkflowInspectorField::TimeoutMs && buffer.parse::<u64>().is_err() {
                state.workflow_view.notice = Some("timeout_ms must be a number".to_string());
                return None;
            }
            state.workflow_view.inspector = None;
            apply_patch(state, field, &buffer)
        }
        KeyCode::Backspace => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .edit
                .as_mut()?
                .buffer
                .pop();
            None
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .edit
                .as_mut()?
                .buffer
                .clear();
            None
        }
        KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
            let edit = state.workflow_view.inspector.as_mut()?.edit.as_mut()?;
            while edit.buffer.ends_with(|ch: char| ch.is_whitespace()) {
                edit.buffer.pop();
            }
            while edit.buffer.ends_with(|ch: char| !ch.is_whitespace()) {
                edit.buffer.pop();
            }
            None
        }
        KeyCode::Char(ch) if plain(&key) => {
            state
                .workflow_view
                .inspector
                .as_mut()?
                .edit
                .as_mut()?
                .buffer
                .push(ch);
            None
        }
        _ => None,
    }
}

/// Build the node patch, dispatch it through the shared API path, and leave
/// a notice about when the edit takes effect.
fn apply_patch(
    state: &mut AppState,
    field: WorkflowInspectorField,
    raw: &str,
) -> Option<WorkflowGraphAction> {
    let (path, node_id) = {
        let view = &state.workflow_view;
        let snapshot = view.open.as_ref()?;
        let node_id = snapshot.nodes.get(view.selection)?.id.clone();
        (snapshot.path.clone(), node_id)
    };
    let mut patch = WorkflowNodePatch {
        node_id,
        runtime: None,
        provider_profile_id: None,
        provider_pool: None,
        model: None,
        timeout_ms: None,
        visible: None,
        enabled: None,
    };
    match field {
        WorkflowInspectorField::Runtime => patch.runtime = Some(raw.to_string()),
        WorkflowInspectorField::Profile => patch.provider_profile_id = Some(raw.to_string()),
        WorkflowInspectorField::Pool => patch.provider_pool = Some(raw.to_string()),
        WorkflowInspectorField::Model => patch.model = Some(raw.to_string()),
        WorkflowInspectorField::TimeoutMs => {
            patch.timeout_ms = Some(raw.parse::<u64>().unwrap_or(0))
        }
        WorkflowInspectorField::Visible => patch.visible = Some(raw == "true"),
        WorkflowInspectorField::Enabled => patch.enabled = Some(raw == "true"),
    }
    let effect = if field.invalidates_cache() {
        "saved; the node re-runs on the next run/resume (cache invalidated)"
    } else {
        "saved (presentation only)"
    };
    state.workflow_view.notice = Some(effect.to_string());
    Some(WorkflowGraphAction::Api(Box::new(Method::WorkflowUpdate(
        WorkflowUpdateParams {
            path,
            patches: vec![patch],
        },
    ))))
}

/// Prime the inspector for a node (App side: masked profiles and pool group
/// names from the registry join here).
pub(crate) fn open_inspector(
    state: &mut AppState,
    node_id: &str,
    profiles: Vec<crate::api::schema::ProviderProfileInfo>,
    pools: Vec<String>,
) {
    state.workflow_view.inspector = Some(WorkflowInspectorState {
        node_id: node_id.to_string(),
        list: SelectionListState::new(0),
        edit: None,
        choice: None,
        profiles,
        pools,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::{Mode, WorkflowGraphSnapshot};

    fn node(id: &str, phase: &str) -> WorkflowNodeView {
        WorkflowNodeView {
            id: id.to_string(),
            title: id.to_string(),
            kind: "agent".to_string(),
            runtime: Some("claude-code".to_string()),
            profile_id: None,
            provider_pool: None,
            model: Some("m".to_string()),
            visible: true,
            enabled: true,
            timeout_ms: 0,
            phase: phase.to_string(),
            cached: false,
            error: None,
            skip_reason: None,
            cost_usd: None,
            tokens: None,
            artifact: None,
            pane: None,
            agent_state: None,
            sort_y: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn view_state_open() -> AppState {
        let mut state = AppState::test_new();
        state.workflow_view.open = Some(WorkflowGraphSnapshot {
            run_id: "r1".to_string(),
            workflow_name: "demo".to_string(),
            path: "/tmp/wf.aflow.json".to_string(),
            status: "running".to_string(),
            live: true,
            workspace_idx: Some(0),
            nodes: vec![node("a", "done"), node("b", "running")],
            // a → b puts the two nodes in separate layout columns.
            edges: vec![("a".to_string(), "b".to_string())],
        });
        state.mode = Mode::WorkflowGraph;
        state
    }

    #[test]
    fn arrows_move_selection() {
        let mut state = view_state_open();
        assert_eq!(state.workflow_view.selection, 0);
        assert!(update_workflow_graph_state(&mut state, key(KeyCode::Right)).is_none());
        assert_eq!(state.workflow_view.selection, 1);
        update_workflow_graph_state(&mut state, key(KeyCode::Left));
        assert_eq!(state.workflow_view.selection, 0);
    }

    #[test]
    fn esc_leaves_mode() {
        let mut state = view_state_open();
        update_workflow_graph_state(&mut state, key(KeyCode::Esc));
        assert_ne!(state.mode, Mode::WorkflowGraph);
    }

    #[test]
    fn enter_without_pane_notifies() {
        let mut state = view_state_open();
        assert!(update_workflow_graph_state(&mut state, key(KeyCode::Enter)).is_none());
        assert!(state
            .workflow_view
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("output & logs")));
    }

    #[test]
    fn pause_toggles_by_status() {
        let mut state = view_state_open();
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('p'))) {
            Some(WorkflowGraphAction::Api(method))
                if matches!(*method, Method::WorkflowPause(_)) => {}
            other => panic!("expected pause, got {other:?}"),
        }
        state.workflow_view.open.as_mut().unwrap().status = "paused".to_string();
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('p'))) {
            Some(WorkflowGraphAction::Api(method))
                if matches!(*method, Method::WorkflowResume(_)) => {}
            other => panic!("expected resume, got {other:?}"),
        }
    }

    #[test]
    fn partial_fail_resumes_with_pause_key() {
        let mut state = view_state_open();
        state.workflow_view.open.as_mut().unwrap().status = "partial_fail".to_string();
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('p'))) {
            Some(WorkflowGraphAction::Api(method))
                if matches!(*method, Method::WorkflowResume(_)) => {}
            other => panic!("expected resume for partial_fail, got {other:?}"),
        }
    }

    #[test]
    fn cancel_requires_two_presses() {
        let mut state = view_state_open();
        assert!(update_workflow_graph_state(&mut state, key(KeyCode::Char('x'))).is_none());
        assert!(state.workflow_view.confirm_cancel);
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('x'))) {
            Some(WorkflowGraphAction::Api(method))
                if matches!(*method, Method::WorkflowCancel(_)) => {}
            other => panic!("expected cancel, got {other:?}"),
        }
        assert!(!state.workflow_view.confirm_cancel);
    }

    #[test]
    fn inspector_edits_model_via_update_api() {
        let mut state = view_state_open();
        open_inspector(&mut state, "a", Vec::new(), Vec::new());
        // Rows: runtime(0) profile(1) pool(2) model(3).
        for _ in 0..3 {
            update_workflow_graph_state(&mut state, key(KeyCode::Down));
        }
        update_workflow_graph_state(&mut state, key(KeyCode::Enter));
        assert!(state
            .workflow_view
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.edit.is_some()));
        for ch in "-4.7".chars() {
            update_workflow_graph_state(&mut state, key(KeyCode::Char(ch)));
        }
        match update_workflow_graph_state(&mut state, key(KeyCode::Enter)) {
            Some(WorkflowGraphAction::Api(boxed)) => {
                let Method::WorkflowUpdate(params) = *boxed else {
                    panic!("expected update");
                };
                assert_eq!(params.patches.len(), 1);
                assert_eq!(params.patches[0].node_id, "a");
                assert_eq!(params.patches[0].model.as_deref(), Some("m-4.7"));
            }
            other => panic!("expected update, got {other:?}"),
        }
        assert!(state.workflow_view.inspector.is_none());
        assert!(state
            .workflow_view
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("cache invalidated")));
    }

    #[test]
    fn inspector_choice_picks_runtime() {
        let mut state = view_state_open();
        open_inspector(&mut state, "a", Vec::new(), Vec::new());
        update_workflow_graph_state(&mut state, key(KeyCode::Enter));
        // Runtime choice list is open; move to grok-build (index 2).
        for _ in 0..2 {
            update_workflow_graph_state(&mut state, key(KeyCode::Down));
        }
        match update_workflow_graph_state(&mut state, key(KeyCode::Enter)) {
            Some(WorkflowGraphAction::Api(boxed)) => {
                let Method::WorkflowUpdate(params) = *boxed else {
                    panic!("expected update");
                };
                assert_eq!(params.patches[0].runtime.as_deref(), Some("grok-build"));
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn inspector_esc_is_layer_local() {
        let mut state = view_state_open();
        open_inspector(&mut state, "a", Vec::new(), Vec::new());
        update_workflow_graph_state(&mut state, key(KeyCode::Enter));
        update_workflow_graph_state(&mut state, key(KeyCode::Esc));
        assert!(state
            .workflow_view
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.choice.is_none()));
        assert_eq!(state.mode, Mode::WorkflowGraph);
        update_workflow_graph_state(&mut state, key(KeyCode::Esc));
        assert!(state.workflow_view.inspector.is_none());
        assert_eq!(state.mode, Mode::WorkflowGraph);
    }

    #[test]
    fn tab_cycles_runs() {
        use crate::app::state::WorkflowRunSummary;
        let mut state = view_state_open();
        let summary = |run_id: &str, path_valid: bool| WorkflowRunSummary {
            run_id: run_id.to_string(),
            workflow_name: format!("wf-{run_id}"),
            status: "done".to_string(),
            started_unix: 0,
            done_count: 1,
            total_nodes: 1,
            path_valid,
        };
        state.workflow_view.runs = vec![summary("r0", true), summary("r1", true)];
        // Plain Tab goes to the next run; Shift+Tab goes back.
        match update_workflow_graph_state(&mut state, key(KeyCode::Tab)) {
            Some(WorkflowGraphAction::SwitchRun { run_id }) => assert_eq!(run_id, "r0"),
            other => panic!("expected switch, got {other:?}"),
        }
        match update_workflow_graph_state(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ) {
            Some(WorkflowGraphAction::SwitchRun { run_id }) => assert_eq!(run_id, "r0"),
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn angle_brackets_cycle_runs_with_wraparound() {
        use crate::app::state::WorkflowRunSummary;
        let mut state = view_state_open();
        let summary = |run_id: &str, path_valid: bool| WorkflowRunSummary {
            run_id: run_id.to_string(),
            workflow_name: format!("wf-{run_id}"),
            status: "done".to_string(),
            started_unix: 0,
            done_count: 1,
            total_nodes: 1,
            path_valid,
        };
        // r2's workflow file is gone: > must skip it and wrap to r0... wait,
        // r0 sits before r1; with r2 invalid, > from r1 wraps past r2 to r0.
        state.workflow_view.runs = vec![
            summary("r0", true),
            summary("r1", true),
            summary("r2", false),
        ];
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('>'))) {
            Some(WorkflowGraphAction::SwitchRun { run_id }) => {
                assert_eq!(run_id, "r0", "invalid runs are skipped")
            }
            other => panic!("expected switch, got {other:?}"),
        }
        state.workflow_view.runs = vec![
            summary("r0", true),
            summary("r1", true),
            summary("r2", true),
        ];
        // The open snapshot is run "r1" (index 1): > goes to r2, < wraps to r0.
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('>'))) {
            Some(WorkflowGraphAction::SwitchRun { run_id }) => assert_eq!(run_id, "r2"),
            other => panic!("expected switch, got {other:?}"),
        }
        match update_workflow_graph_state(&mut state, key(KeyCode::Char('<'))) {
            Some(WorkflowGraphAction::SwitchRun { run_id }) => assert_eq!(run_id, "r0"),
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn timeout_rejects_non_numeric() {
        let mut state = view_state_open();
        open_inspector(&mut state, "a", Vec::new(), Vec::new());
        // Rows: runtime(0) profile(1) pool(2) model(3) timeout(4).
        for _ in 0..4 {
            update_workflow_graph_state(&mut state, key(KeyCode::Down));
        }
        update_workflow_graph_state(&mut state, key(KeyCode::Enter));
        update_workflow_graph_state(
            &mut state,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        update_workflow_graph_state(&mut state, key(KeyCode::Char('x')));
        update_workflow_graph_state(&mut state, key(KeyCode::Enter));
        assert!(
            state
                .workflow_view
                .inspector
                .as_ref()
                .is_some_and(|inspector| inspector.edit.is_some()),
            "invalid timeout keeps the editor open"
        );
        assert!(state
            .workflow_view
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("number")));
    }
}
