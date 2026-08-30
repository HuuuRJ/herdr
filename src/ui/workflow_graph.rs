//! Workflow graph view rendering: DAG cards with phase coloring, ASCII
//! connectors, run banner, key footer, and the node inspector modal.
//! Everything reads the AppState projection (`workflow_view`); nothing here
//! touches the engine or the filesystem.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::widgets::{panel_contrast_fg, render_modal_shell, render_panel_shell};
use crate::app::state::{
    AppState, WorkflowGraphSnapshot, WorkflowInspectorState, WorkflowNodeView,
};
use crate::workflow::graph::{self, CARD_HEIGHT, ROW_PITCH};

const INSPECTOR_WIDTH: u16 = 64;
const INSPECTOR_FIELDS: [&str; 6] = [
    "runtime",
    "profile",
    "model",
    "timeout_ms",
    "visible",
    "enabled",
];

pub(super) fn render_workflow_graph_overlay(app: &AppState, frame: &mut Frame) {
    let Some(snapshot) = app.workflow_view.open.as_ref() else {
        return;
    };
    let popup = app.navigator_popup_rect();
    let Some(inner) = render_panel_shell(frame, popup, app.palette.accent, app.palette.panel_bg)
    else {
        return;
    };
    let p = &app.palette;
    let header = Rect::new(inner.x, inner.y, inner.width, inner.height.min(1));
    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        inner.height.min(1),
    );
    let body = if inner.height > 2 {
        Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(2),
        )
    } else {
        Rect::default()
    };

    let done = snapshot.nodes.iter().filter(|n| n.phase == "done").count();
    let (status_color, status_label) = run_status_style(&snapshot.status);
    // Position among openable runs: Tab/Shift+Tab cycling needs a visible
    // "where am I" cue because same-named runs render identical graphs.
    let openable: Vec<&String> = app
        .workflow_view
        .runs
        .iter()
        .filter(|run| run.path_valid)
        .map(|run| &run.run_id)
        .collect();
    let position = openable
        .iter()
        .position(|run_id| *run_id == &snapshot.run_id)
        .map(|index| format!(" · [{}/{}]", index + 1, openable.len()))
        .unwrap_or_default();
    let spans = vec![
        Span::styled(
            format!(" wf:{}", snapshot.workflow_name),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — ", Style::default().fg(p.overlay0)),
        Span::styled(status_label, Style::default().fg(status_color)),
        Span::styled(
            format!(
                "{position} · {done}/{} {} · {}",
                snapshot.nodes.len(),
                if snapshot.live { "live" } else { "history" },
                snapshot.run_id
            ),
            Style::default().fg(p.overlay0),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    if body.width > 0 && body.height > 0 {
        render_graph(app, snapshot, frame, body);
    }

    let view = &app.workflow_view;
    let footer_text = if let Some(notice) = &view.notice {
        format!(" {notice}")
    } else if view.confirm_cancel {
        " press x again to cancel · esc disarms".to_string()
    } else {
        " ←→↑↓ select · tab/shift+tab runs · enter pane · i inspect · p pause/resume · x cancel · esc close"
            .to_string()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            footer_text,
            Style::default().fg(if view.notice.is_some() {
                p.yellow
            } else {
                p.overlay0
            }),
        )),
        footer,
    );

    if let Some(inspector) = &app.workflow_view.inspector {
        render_inspector(app, snapshot, inspector, frame, popup);
    }
}

fn run_status_style(status: &str) -> (ratatui::style::Color, String) {
    // Palette colors are resolved by the caller; keep this fn textual.
    match status {
        "running" => (ratatui::style::Color::Yellow, "running".to_string()),
        "paused" => (ratatui::style::Color::LightYellow, "paused".to_string()),
        "done" => (ratatui::style::Color::Green, "done".to_string()),
        "error" => (ratatui::style::Color::Red, "error".to_string()),
        "cancelled" => (ratatui::style::Color::DarkGray, "cancelled".to_string()),
        other => (ratatui::style::Color::Gray, other.to_string()),
    }
}

/// Transitive structural skip (node disabled or downstream of one), from
/// the snapshot's edge list — mirrors `WorkflowDef::is_structurally_skipped`.
fn structurally_skipped(snapshot: &WorkflowGraphSnapshot, id: &str) -> bool {
    let disabled = |node: &WorkflowNodeView| !node.enabled;
    if snapshot
        .nodes
        .iter()
        .any(|node| node.id == id && disabled(node))
    {
        return true;
    }
    let mut stack: Vec<&str> = snapshot
        .edges
        .iter()
        .filter(|(_, to)| to == id)
        .map(|(from, _)| from.as_str())
        .collect();
    let mut seen = std::collections::HashSet::new();
    while let Some(next) = stack.pop() {
        if seen.insert(next.to_string()) {
            if snapshot
                .nodes
                .iter()
                .any(|node| node.id == next && disabled(node))
            {
                return true;
            }
            stack.extend(
                snapshot
                    .edges
                    .iter()
                    .filter(|(_, to)| to == next)
                    .map(|(from, _)| from.as_str()),
            );
        }
    }
    false
}

fn render_graph(app: &AppState, snapshot: &WorkflowGraphSnapshot, frame: &mut Frame, body: Rect) {
    let p = &app.palette;
    let layout = snapshot.graph_layout();
    let view = &app.workflow_view;
    let (scroll_x, scroll_y) = (view.scroll_x as u16, view.scroll_y as u16);

    let card_anchor = |id: &str| -> Option<(u16, u16, u16)> {
        let card = layout.card(id)?;
        Some((
            layout.col_x[card.col] as u16,
            (card.row * ROW_PITCH) as u16,
            card.width as u16,
        ))
    };

    // Connectors first — cards paint over them. The selected node's edges
    // light up in the accent color so upstream/downstream read at a glance.
    let selected_id = snapshot
        .nodes
        .get(view.selection)
        .map(|node| node.id.clone());
    let buf = frame.buffer_mut();
    for (from, to) in &snapshot.edges {
        let Some((fx, fy, fw)) = card_anchor(from) else {
            continue;
        };
        let Some((tx, ty, _)) = card_anchor(to) else {
            continue;
        };
        let exit = (
            (fx + fw).saturating_sub(scroll_x),
            fy + CARD_HEIGHT as u16 / 2 - scroll_y,
        );
        // The arrow lands one cell before the target card so the card's own
        // border does not paint over it.
        let entry = (
            tx.saturating_sub(scroll_x).saturating_sub(1),
            ty + CARD_HEIGHT as u16 / 2 - scroll_y,
        );
        let highlighted = selected_id.as_deref() == Some(from.as_str())
            || selected_id.as_deref() == Some(to.as_str());
        let connector_style = if highlighted {
            Style::default().fg(p.accent)
        } else {
            Style::default().fg(p.overlay1)
        };
        for (x, y, cell) in graph::connector_cells(
            (exit.0 as usize, exit.1 as usize),
            (entry.0 as usize, entry.1 as usize),
        ) {
            // Connector cells arrive in viewport coordinates (scroll already
            // subtracted); translate them onto the screen before painting.
            let (screen_x, screen_y) = (body.x + x as u16, body.y + y as u16);
            if screen_x >= body.x
                && screen_x < body.x + body.width
                && screen_y >= body.y
                && screen_y < body.y + body.height
            {
                if let Some(buffer_cell) = buf.cell_mut((screen_x, screen_y)) {
                    buffer_cell
                        .set_symbol(cell.symbol())
                        .set_style(connector_style);
                }
            }
        }
    }

    // Cards.
    for (index, node) in snapshot.nodes.iter().enumerate() {
        let Some((vx, vy, vw)) = card_anchor(&node.id) else {
            continue;
        };
        let card = Rect::new(
            body.x + vx.saturating_sub(scroll_x),
            body.y + vy.saturating_sub(scroll_y),
            vw,
            CARD_HEIGHT as u16,
        );
        let clipped = card.intersection(body);
        if clipped.width == 0 || clipped.height == 0 {
            continue;
        }
        let selected = index == view.selection;
        // Selected cards invert (accent background + contrast text) like a
        // highlighted settings row; otherwise the border carries the phase.
        let border = if selected {
            p.accent
        } else {
            match node.phase.as_str() {
                "done" => p.green,
                "running" => p.yellow,
                "error" => p.red,
                _ => p.overlay0,
            }
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .style(if selected {
                Style::default().bg(p.accent)
            } else {
                Style::default().bg(p.panel_bg)
            });
        frame.render_widget(Clear, clipped);
        frame.render_widget(block, clipped);
        let inner = Rect::new(
            card.x + 1,
            card.y + 1,
            card.width.saturating_sub(2),
            CARD_HEIGHT as u16 - 2,
        )
        .intersection(body);
        if inner.width == 0 || inner.height == 0 {
            continue;
        }
        let marker = if selected { "▸ " } else { "  " };
        let content_fg = if selected {
            panel_contrast_fg(p)
        } else {
            p.text
        };
        let title = Line::from(Span::styled(
            format!("{marker}{}", node.title),
            Style::default().fg(content_fg).add_modifier(Modifier::BOLD),
        ));
        let meta = Line::from(Span::styled(
            format!("  {}", node.runtime.as_deref().unwrap_or(&node.kind)),
            Style::default().fg(if selected { content_fg } else { p.overlay0 }),
        ));
        frame.render_widget(Paragraph::new(title), inner);
        if inner.height > 1 {
            frame.render_widget(
                Paragraph::new(meta),
                Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
            );
        }
        if inner.height > 2 {
            let mut status = status_line(p, node, structurally_skipped(snapshot, &node.id));
            if selected {
                // One flat color on the inverted card; semantic colors would
                // fight the accent background.
                for span in &mut status.spans {
                    span.style = Style::default().fg(content_fg);
                }
            }
            frame.render_widget(
                Paragraph::new(status),
                Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2),
            );
        }
    }
}

fn status_line(
    p: &crate::app::state::Palette,
    node: &WorkflowNodeView,
    skipped: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if skipped {
        spans.push(Span::styled("⊘ skipped", Style::default().fg(p.overlay0)));
        return Line::from(spans);
    }
    match node.phase.as_str() {
        "done" => {
            spans.push(Span::styled("✓", Style::default().fg(p.green)));
            if node.cached {
                spans.push(Span::styled("·cache", Style::default().fg(p.overlay0)));
            }
        }
        "running" => {
            spans.push(Span::styled("●", Style::default().fg(p.yellow)));
            // Agent-detection overlay (W5: UI coloring only).
            match node.agent_state {
                Some(crate::detect::AgentState::Blocked) => {
                    spans.push(Span::styled(" blocked", Style::default().fg(p.red)))
                }
                Some(crate::detect::AgentState::Working) => {
                    spans.push(Span::styled(" working", Style::default().fg(p.green)))
                }
                Some(crate::detect::AgentState::Idle) => {
                    spans.push(Span::styled(" idle", Style::default().fg(p.overlay0)))
                }
                _ => {}
            }
        }
        "error" => spans.push(Span::styled("✗", Style::default().fg(p.red))),
        _ => spans.push(Span::styled("·", Style::default().fg(p.overlay0))),
    }
    if let Some(cost) = node.cost_usd {
        spans.push(Span::styled(
            format!(" {cost:.2}¢"),
            Style::default().fg(p.overlay0),
        ));
    }
    if let Some(tokens) = node.tokens {
        spans.push(Span::styled(
            format!(" {tokens}t"),
            Style::default().fg(p.overlay0),
        ));
    }
    Line::from(spans)
}

fn render_inspector(
    app: &AppState,
    snapshot: &WorkflowGraphSnapshot,
    inspector: &WorkflowInspectorState,
    frame: &mut Frame,
    area: Rect,
) {
    let p = &app.palette;
    let Some(node) = snapshot.node(&inspector.node_id) else {
        return;
    };
    if let Some(choice) = &inspector.choice {
        let height = 4 + choice.options.len() as u16;
        let Some(inner) =
            render_modal_shell(frame, area, INSPECTOR_WIDTH, height.min(area.height), p)
        else {
            return;
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} · choose {}", node.title, choice.field.label()),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ))),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        for (index, option) in choice.options.iter().enumerate() {
            let selected = index == choice.list.selected;
            let row = Rect::new(inner.x, inner.y + 2 + index as u16, inner.width, 1);
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(format!("{marker}{option}"), style)),
                row,
            );
        }
        let hint = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " enter apply · esc back",
                Style::default().fg(p.overlay0),
            )),
            hint,
        );
        return;
    }
    if let Some(edit) = &inspector.edit {
        let Some(inner) = render_modal_shell(frame, area, INSPECTOR_WIDTH, 6, p) else {
            return;
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} · edit {}", node.title, edit.field.label()),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ))),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}▏", edit.buffer),
                Style::default().fg(p.text),
            )),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " enter save · esc back · ctrl+u clear",
                Style::default().fg(p.overlay0),
            )),
            Rect::new(inner.x, inner.y + 4, inner.width, 1),
        );
        return;
    }

    let read_only = format!(
        " {} · {} · run {}",
        node.title,
        node.kind,
        if snapshot.live { "live" } else { "history" }
    );
    let height = 5 + INSPECTOR_FIELDS.len() as u16;
    let Some(inner) = render_modal_shell(frame, area, INSPECTOR_WIDTH, height, p) else {
        return;
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            read_only,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let values = [
        node.runtime.clone().unwrap_or_else(|| "—".into()),
        node.profile_id
            .clone()
            .unwrap_or_else(|| "(unbound)".into()),
        node.model.clone().unwrap_or_else(|| "—".into()),
        if node.timeout_ms == 0 {
            "0 (none)".to_string()
        } else {
            node.timeout_ms.to_string()
        },
        node.visible.to_string(),
        node.enabled.to_string(),
    ];
    for (index, (field, value)) in INSPECTOR_FIELDS.iter().zip(values).enumerate() {
        let selected = index == inspector.list.selected;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        let cache_note = if selected && matches!(*field, "runtime" | "profile" | "model") {
            Span::styled("  (resets cache)", Style::default().fg(p.yellow))
        } else {
            Span::raw("")
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{marker}{field:<11}"), style),
                Span::styled(value, Style::default().fg(p.text)),
                cache_note,
            ])),
            Rect::new(inner.x, inner.y + 2 + index as u16, inner.width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            " enter edit · esc back (prompt text is edited in the file)",
            Style::default().fg(p.overlay0),
        )),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::{Mode, WorkflowGraphSnapshot, WorkflowNodeView};
    use ratatui::{backend::TestBackend, Terminal as TestTerminal};

    fn node(id: &str, runtime: &str) -> WorkflowNodeView {
        WorkflowNodeView {
            id: id.to_string(),
            title: id.to_string(),
            kind: "agent".to_string(),
            runtime: Some(runtime.to_string()),
            profile_id: None,
            model: None,
            visible: true,
            enabled: true,
            timeout_ms: 0,
            phase: "done".to_string(),
            cached: false,
            error: None,
            cost_usd: Some(0.12),
            tokens: Some(7),
            artifact: None,
            pane: None,
            agent_state: None,
            sort_y: None,
        }
    }

    fn draw_graph(
        selected: usize,
    ) -> (
        ratatui::buffer::Buffer,
        ratatui::style::Color,
        ratatui::style::Color,
    ) {
        let mut app = AppState::test_new();
        app.workflow_view.open = Some(WorkflowGraphSnapshot {
            run_id: "r1".to_string(),
            workflow_name: "t".to_string(),
            path: "/t.aflow.json".to_string(),
            status: "done".to_string(),
            live: false,
            workspace_idx: None,
            nodes: vec![node("alpha", "claude-code"), node("beta", "grok-build")],
            edges: vec![("alpha".to_string(), "beta".to_string())],
        });
        app.workflow_view.selection = selected;
        app.mode = Mode::WorkflowGraph;
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 100, 40));
        let text_color = app.palette.text;
        let contrast_color = panel_contrast_fg(&app.palette);
        let mut terminal = TestTerminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| render_workflow_graph_overlay(&app, frame))
            .unwrap();
        (
            terminal.backend().buffer().clone(),
            text_color,
            contrast_color,
        )
    }

    /// The graph text: one row per line so substring search works.
    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let width = buffer.area().width as usize;
        let mut rows = Vec::new();
        let mut current = String::new();
        for (index, cell) in buffer.content().iter().enumerate() {
            if index > 0 && index % width == 0 {
                rows.push(std::mem::take(&mut current));
            }
            current.push_str(cell.symbol());
        }
        rows.push(current);
        rows
    }

    #[test]
    fn connectors_render_between_cards() {
        let (buffer, _, _) = draw_graph(0);
        let text = buffer_text(&buffer).join("\n");
        assert!(text.contains('─'), "horizontal connector cells visible");
        assert!(text.contains('▶'), "connector arrowhead visible");
    }

    #[test]
    fn selected_card_inverts_while_titles_stay_visible() {
        let (buffer, text_color, contrast_color) = draw_graph(0);
        // Match "alpha" by cell index: wide glyphs leave empty follow-up
        // symbols that would shift any char-based search.
        let find_cells = |word: &[&str]| -> Option<usize> {
            let content = buffer.content();
            'outer: for start in 0..content.len().saturating_sub(word.len()) {
                for (offset, expected) in word.iter().enumerate() {
                    if content[start + offset].symbol() != *expected {
                        continue 'outer;
                    }
                }
                return Some(start);
            }
            None
        };
        let alpha = find_cells(&["a", "l", "p", "h", "a"]).expect("selected title rendered");
        assert_eq!(
            buffer.content()[alpha].style().fg,
            Some(contrast_color),
            "selected title uses the inverted contrast color"
        );
        let beta = find_cells(&["b", "e", "t", "a"]).expect("unselected title rendered");
        assert_eq!(buffer.content()[beta].style().fg, Some(text_color));
    }
}
