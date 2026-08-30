//! Pure DAG layout for the TUI workflow graph view.
//!
//! Columns are always topological (Kahn longest-path layering) so every
//! connector flows left → right; canvas `position.y` values order nodes
//! within a column when present, file order otherwise. All geometry is in
//! virtual cells; the renderer maps them onto the screen with scroll
//! offsets and paints cards over connectors.

use super::model::{NodeType, WorkflowDef, WorkflowNode};

pub(crate) const CARD_HEIGHT: usize = 5;
pub(crate) const ROW_PITCH: usize = CARD_HEIGHT + 1;
pub(crate) const COL_GAP: usize = 4;
pub(crate) const CARD_MIN_WIDTH: usize = 12;
pub(crate) const CARD_MAX_WIDTH: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphNodeCard {
    pub id: String,
    pub col: usize,
    pub row: usize,
    pub width: usize,
    pub title: String,
    pub meta: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GraphLayout {
    pub cards: Vec<GraphNodeCard>,
    /// x offset of each column (index = column).
    pub col_x: Vec<usize>,
    /// Virtual canvas size in cells.
    pub width: usize,
    pub height: usize,
}

impl GraphLayout {
    pub(crate) fn card(&self, id: &str) -> Option<&GraphNodeCard> {
        self.cards.iter().find(|card| card.id == id)
    }
}

/// Minimal node description the layout consumes — the def adapter and the
/// graph-view snapshot projection both produce these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutNode {
    pub id: String,
    pub title: String,
    pub meta: String,
    pub sort_y: Option<i64>,
    pub deps: Vec<String>,
}

/// One-line kind/meta label under the title.
#[cfg_attr(not(test), allow(dead_code))]
fn node_meta(node: &WorkflowNode) -> String {
    match node.node_type {
        NodeType::Agent => {
            let runtime = node
                .runtime
                .map(|runtime| runtime.label())
                .unwrap_or("agent");
            match &node.model {
                Some(model) => format!("{runtime}·{model}"),
                None => runtime.to_string(),
            }
        }
        NodeType::PromptTemplate => "template".to_string(),
        NodeType::ImageGen => "image".to_string(),
    }
}

/// Elide to `limit` display characters with a trailing ellipsis.
fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let prefix: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{prefix}…")
}

/// def adapter for the layout core (the def path is exercised by tests; the
/// graph view projects snapshots through `layout_nodes` directly).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn layout(def: &WorkflowDef) -> GraphLayout {
    let nodes: Vec<LayoutNode> = def
        .nodes
        .iter()
        .map(|node| LayoutNode {
            id: node.id.clone(),
            title: node.display_title().to_string(),
            meta: node_meta(node),
            sort_y: node.position.as_ref().map(|position| position.y),
            deps: node.after.clone(),
        })
        .collect();
    layout_nodes(&nodes)
}

pub(crate) fn layout_nodes(nodes: &[LayoutNode]) -> GraphLayout {
    // Kahn longest-path layering: a node's column is 1 + max(dep column).
    // Validation guarantees acyclicity, so this terminates.
    let mut col_of: Vec<(String, usize)> = Vec::with_capacity(nodes.len());
    let mut pending: Vec<&LayoutNode> = nodes.iter().collect();
    while !pending.is_empty() {
        let mut progressed = false;
        let mut next = Vec::new();
        for node in pending {
            let ready = node
                .deps
                .iter()
                .all(|dep| col_of.iter().any(|(id, _)| id == dep));
            if ready {
                let col = node
                    .deps
                    .iter()
                    .filter_map(|dep| col_of.iter().find(|(id, _)| id == dep).map(|(_, col)| *col))
                    .max()
                    .map_or(0, |col| col + 1);
                col_of.push((node.id.clone(), col));
                progressed = true;
            } else {
                next.push(node);
            }
        }
        if !progressed {
            // Defensive: a cycle slipped past validation; pack leftovers in
            // the last column so rendering still terminates.
            let last = col_of
                .iter()
                .map(|(_, col)| *col)
                .max()
                .map_or(0, |col| col);
            for node in next {
                col_of.push((node.id.clone(), last));
            }
            break;
        }
        pending = next;
    }

    let total_cols = col_of
        .iter()
        .map(|(_, col)| *col)
        .max()
        .map_or(0, |col| col + 1);
    // Cards first (rows assigned per column below).
    let mut cards: Vec<GraphNodeCard> = Vec::with_capacity(nodes.len());
    for (id, col) in &col_of {
        let node = nodes
            .iter()
            .find(|node| &node.id == id)
            .expect("col_of mirrors nodes");
        let title = elide(&node.title, CARD_MAX_WIDTH - 4);
        let meta = elide(&node.meta, CARD_MAX_WIDTH - 4);
        let width = (CARD_MIN_WIDTH)
            .max(title.chars().count() + 4)
            .max(meta.chars().count() + 4)
            .min(CARD_MAX_WIDTH);
        cards.push(GraphNodeCard {
            id: id.clone(),
            col: *col,
            row: 0,
            width,
            title,
            meta,
        });
    }
    // Assign rows per column: sort_y when present, input order otherwise.
    for col in 0..total_cols {
        let mut in_col: Vec<(i64, usize)> = cards
            .iter()
            .enumerate()
            .filter(|(_, card)| card.col == col)
            .map(|(index, card)| {
                let node = &nodes
                    .iter()
                    .find(|node| node.id == card.id)
                    .expect("cards mirror nodes");
                (node.sort_y.unwrap_or(index as i64), index)
            })
            .collect();
        in_col.sort();
        for (row, (_, index)) in in_col.into_iter().enumerate() {
            cards[index].row = row;
        }
    }

    // Column x offsets from the widest card per column.
    let mut col_widths = vec![CARD_MIN_WIDTH; total_cols];
    for card in &cards {
        col_widths[card.col] = col_widths[card.col].max(card.width);
    }
    let mut col_x = Vec::with_capacity(total_cols);
    let mut x = 0;
    for width in &col_widths {
        col_x.push(x);
        x += width + COL_GAP;
    }
    let width = x.saturating_sub(COL_GAP).max(1);
    let height = cards
        .iter()
        .map(|card| (card.row + 1) * ROW_PITCH)
        .max()
        .unwrap_or(ROW_PITCH);
    GraphLayout {
        cards,
        col_x,
        width,
        height,
    }
}

/// Edge list derived from `after` dependencies: `(upstream, downstream)`.
pub(crate) fn edges(def: &WorkflowDef) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for node in &def.nodes {
        for dep in &node.after {
            edges.push((dep.clone(), node.id.clone()));
        }
    }
    edges
}

/// Cells for one connector: exit the source's right edge at its vertical
/// center, run to the midpoint between the two cards, turn vertically, then
/// enter the target's left edge. Cells may collide with other connectors;
/// the renderer merges overlapping segments.
pub(crate) fn connector_cells(
    from: (usize, usize),
    to: (usize, usize),
) -> Vec<(usize, usize, ConnCell)> {
    let mut cells = Vec::new();
    let (x1, y1) = from;
    let (x2, y2) = to;
    if x1 >= x2 {
        return cells;
    }
    let mid_x = x1 + (x2 - x1) / 2;
    // Horizontal out of the source.
    for x in x1..mid_x {
        cells.push((x, y1, ConnCell::Horizontal));
    }
    // Vertical turn (same-row edges stay plain horizontals — no stray stem).
    let (low, high) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
    for y in low..=high {
        let vertical = y != y1 && y != y2;
        let horizontal = y == y1 || y == y2;
        let cell = match (horizontal, vertical) {
            (true, true) => ConnCell::Cross,
            (true, false) => ConnCell::Horizontal,
            (false, _) => ConnCell::Vertical,
        };
        cells.push((mid_x, y, cell));
    }
    // Horizontal into the target (arrowhead occupies the target's edge).
    for x in mid_x + 1..x2 {
        cells.push((x, y2, ConnCell::Horizontal));
    }
    cells.push((x2, y2, ConnCell::Arrow));
    cells
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnCell {
    Horizontal,
    Vertical,
    Cross,
    Arrow,
}

impl ConnCell {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Horizontal => "─",
            Self::Vertical => "│",
            Self::Cross => "┼",
            Self::Arrow => "▶",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::WorkflowDef;

    fn three_column_def() -> WorkflowDef {
        WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "source", "type": "prompt_template", "template": "s"},
                {"id": "agent", "type": "agent", "runtime": "grok-build", "model": "grok-4", "prompt": "{{source.output}}", "after": ["source"]},
                {"id": "sink", "type": "prompt_template", "template": "{{agent.output}}", "after": ["agent"]}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn columns_are_topological() {
        let layout = layout(&three_column_def());
        assert_eq!(layout.card("source").unwrap().col, 0);
        assert_eq!(layout.card("agent").unwrap().col, 1);
        assert_eq!(layout.card("sink").unwrap().col, 2);
        assert_eq!(layout.col_x.len(), 3);
        // Columns advance by card width plus the gap.
        assert!(layout.col_x[1] > layout.col_x[0]);
        assert!(layout.card("agent").unwrap().meta.contains("grok-build"));
        assert!(layout.card("agent").unwrap().meta.contains("grok-4"));
    }

    #[test]
    fn position_y_orders_rows_within_a_column() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "low", "type": "prompt_template", "template": "t", "position": {"x": 0, "y": 5}},
                {"id": "high", "type": "prompt_template", "template": "t", "position": {"x": 0, "y": 1}}
            ]}"#,
        )
        .unwrap();
        let layout = layout(&def);
        assert_eq!(layout.card("high").unwrap().row, 0);
        assert_eq!(layout.card("low").unwrap().row, 1);
    }

    #[test]
    fn parallel_nodes_stack_rows() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "t"},
                {"id": "b", "type": "prompt_template", "template": "t"},
                {"id": "c", "type": "agent", "runtime": "dsh", "prompt": "{{a.output}} {{b.output}}", "after": ["a", "b"]}
            ]}"#,
        )
        .unwrap();
        let layout = layout(&def);
        let (row_a, row_b) = (layout.card("a").unwrap().row, layout.card("b").unwrap().row);
        assert_ne!(row_a, row_b, "parallel sources occupy distinct rows");
        assert_eq!(layout.card("c").unwrap().col, 1);
        // Two rows of cards plus one gap row per card band.
        assert!(layout.height >= 2 * ROW_PITCH);
    }

    #[test]
    fn edges_follow_after_lists() {
        let def = three_column_def();
        let mut list = edges(&def);
        list.sort();
        assert_eq!(
            list,
            vec![
                ("agent".to_string(), "sink".to_string()),
                ("source".to_string(), "agent".to_string()),
            ]
        );
    }

    #[test]
    fn connector_path_walks_h_v_h() {
        let cells = connector_cells((10, 1), (20, 5));
        assert!(cells.contains(&(10, 1, ConnCell::Horizontal)));
        assert!(cells.contains(&(15, 3, ConnCell::Vertical)));
        assert!(cells.contains(&(19, 5, ConnCell::Horizontal)));
        assert_eq!(*cells.last().unwrap(), (20, 5, ConnCell::Arrow));
        // Degenerate: target left of source draws nothing.
        assert!(connector_cells((10, 1), (5, 1)).is_empty());
    }

    #[test]
    fn same_row_connector_has_no_vertical_stem() {
        let cells = connector_cells((10, 2), (20, 2));
        assert!(cells
            .iter()
            .all(|(_, _, cell)| matches!(cell, ConnCell::Horizontal | ConnCell::Arrow)));
        assert!(!cells.contains(&(15, 2, ConnCell::Cross)));
    }
}
