//! Workflow file schema and validation.
//!
//! A workflow is a JSON file (`*.aflow.json`) with a name and a node list.
//! Edges are per-node `after` lists (simpler than edge objects, and a future
//! canvas can render them losslessly). Node ids are user-chosen and stable —
//! they anchor the content-hash cache across edits.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<WorkflowNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeType {
    Agent,
    PromptTemplate,
    ImageGen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AgentRuntime {
    ClaudeCode,
    Codex,
    GrokBuild,
    Dsh,
    Custom,
}

impl AgentRuntime {
    /// Wire id (matches the serde kebab-case spelling).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GrokBuild => "grok-build",
            Self::Dsh => "dsh",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionLevel {
    Readonly,
    Workspace,
    Full,
}

impl PermissionLevel {
    /// claude `--permission-mode` mapping (AgentFlow FR-8.3, field-tested).
    pub(crate) fn claude_flag(self) -> &'static str {
        match self {
            Self::Readonly => "plan",
            Self::Workspace => "acceptEdits",
            Self::Full => "bypassPermissions",
        }
    }

    /// codex sandbox flag mapping.
    pub(crate) fn codex_args(self) -> &'static [&'static str] {
        match self {
            Self::Readonly => &["-s", "read-only"],
            Self::Workspace => &["-s", "workspace-write"],
            Self::Full => &["--yolo"],
        }
    }

    /// dsh `DSH_PERMISSION_MODE` mapping (FR-8.3; the headless default is
    /// already workspace-write, so that tier sets nothing).
    pub(crate) fn dsh_env(self) -> Option<&'static str> {
        match self {
            Self::Readonly => Some("read-only"),
            Self::Workspace => None,
            Self::Full => Some("danger-full-access"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NodePosition {
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkflowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Canvas-reserved coordinates; the engine ignores them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<NodePosition>,
    /// Dependency ids (incoming edges).
    #[serde(default)]
    pub after: Vec<String>,

    // -- common execution options --
    /// Run in a visible pane (true) or a background process (false).
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Disabled nodes are structurally skipped: empty output port, no spawn;
    /// the engine cascades the skip downstream.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tolerate this node failing: downstream is skipped instead of the run
    /// failing outright; the run settles into `partial_fail` (FR-5.2).
    /// False is omitted from the serialized form so pre-existing cache
    /// hashes stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_on_error: bool,
    /// 0 = no timeout.
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retry: u32,

    // -- agent --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionLevel>,

    // -- prompt_template --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,

    // -- image_gen --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl WorkflowNode {
    pub(crate) fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }

    /// The effect-free display options that must NOT invalidate the
    /// content-hash cache: moving a node on a canvas, switching pane
    /// visibility, or changing its timeout changes presentation, not output.
    fn is_cache_presentation_field(field: &str) -> bool {
        matches!(
            field,
            "title" | "position" | "visible" | "timeout_ms" | "retry"
        )
    }

    /// Canonical, cache-relevant projection of the node. Hashed by
    /// `runs::node_config_hash`.
    pub(crate) fn cache_projection(&self) -> serde_json::Value {
        let full = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let mut object = serde_json::Map::new();
        if let serde_json::Value::Object(fields) = full {
            for (key, value) in fields {
                if Self::is_cache_presentation_field(&key) {
                    continue;
                }
                if value.is_null() {
                    continue;
                }
                object.insert(key, value);
            }
        }
        serde_json::Value::Object(object)
    }
}

impl WorkflowDef {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let def: WorkflowDef =
            serde_json::from_str(text).map_err(|err| format!("invalid workflow JSON: {err}"))?;
        def.validate()
            .map_err(|errors| format!("invalid workflow: {}", errors.join("; ")))?;
        Ok(def)
    }

    pub(crate) fn node(&self, id: &str) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Collect every validation error (reported together, not first-wins).
    pub(crate) fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if self.name.trim().is_empty() {
            errors.push("workflow name must not be empty".to_string());
        }
        let mut ids: Vec<&str> = self.nodes.iter().map(|node| node.id.as_str()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != total {
            errors.push("node ids must be unique".to_string());
        }

        let ids: Vec<&str> = self.nodes.iter().map(|node| node.id.as_str()).collect();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                errors.push("node id must not be empty".to_string());
            }
            for dep in &node.after {
                if dep == &node.id {
                    errors.push(format!("node '{}' depends on itself", node.id));
                } else if !ids.contains(&dep.as_str()) {
                    errors.push(format!(
                        "node '{}' references unknown dependency '{dep}'",
                        node.id
                    ));
                }
            }
            match node.node_type {
                NodeType::Agent => {
                    if node.prompt.as_deref().unwrap_or("").trim().is_empty()
                        && node.runtime != Some(AgentRuntime::Custom)
                    {
                        errors.push(format!("agent node '{}' requires a prompt", node.id));
                    }
                    match node.runtime {
                        None => errors.push(format!(
                            "agent node '{}' requires a runtime (claude-code, codex, grok-build, dsh, or custom)",
                            node.id
                        )),
                        Some(AgentRuntime::Custom)
                            if node
                                .custom_command
                                .as_deref()
                                .unwrap_or("")
                                .trim()
                                .is_empty() =>
                        {
                            errors.push(format!(
                                "custom agent node '{}' requires custom_command",
                                node.id
                            ));
                        }
                        // {{prompt}} in the template needs a prompt field to
                        // expand from (after {{upstream.output}} rendering).
                        Some(AgentRuntime::Custom)
                            if node
                                .custom_command
                                .as_deref()
                                .unwrap_or("")
                                .contains("{{prompt}}")
                                && node.prompt.as_deref().unwrap_or("").trim().is_empty() =>
                        {
                            errors.push(format!(
                                "custom agent node '{}' uses {{{{prompt}}}} but has no prompt",
                                node.id
                            ));
                        }
                        Some(_) => {}
                    }
                }
                NodeType::PromptTemplate => {
                    if node.template.as_deref().unwrap_or("").trim().is_empty() {
                        errors.push(format!(
                            "prompt_template node '{}' requires a template",
                            node.id
                        ));
                    }
                }
                NodeType::ImageGen => {
                    if node.prompt.as_deref().unwrap_or("").trim().is_empty() {
                        errors.push(format!("image_gen node '{}' requires a prompt", node.id));
                    }
                    if node.output_file.as_deref().unwrap_or("").trim().is_empty() {
                        errors.push(format!("image_gen node '{}' requires output_file", node.id));
                    }
                }
            }
            for reference in template_references(node) {
                if reference == node.id {
                    errors.push(format!("node '{}' references its own output", node.id));
                } else if !ids.contains(&reference.as_str()) {
                    errors.push(format!(
                        "node '{}' references unknown output '{{{{{reference}.output}}}}'",
                        node.id
                    ));
                }
            }
        }

        // Cycle detection via Kahn's algorithm; self-deps and duplicate ids
        // above already poison the accounting so skip when present.
        let graph_is_well_formed = !errors
            .iter()
            .any(|error| error.contains("depends on itself") || error.contains("must be unique"));
        if graph_is_well_formed {
            if let Some(cycle) = self.find_cycle() {
                errors.push(format!(
                    "workflow graph has a cycle: {}",
                    cycle.join(" -> ")
                ));
            }
        }

        // Template references must be upstream (transitive dependencies) so
        // the value exists at render time.
        for node in &self.nodes {
            let closure = self.transitive_deps(&node.id);
            for reference in template_references(node) {
                if reference != node.id && !closure.contains(&reference) {
                    errors.push(format!(
                        "node '{{{0}}}' references '{{{{{reference}.output}}}}' but does not depend on '{reference}' (add it to after)",
                        node.id
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Kahn cycle detection; returns one cycle path as node ids.
    fn find_cycle(&self) -> Option<Vec<String>> {
        let mut remaining_deps: std::collections::HashMap<&str, usize> = self
            .nodes
            .iter()
            .map(|node| {
                let count = node
                    .after
                    .iter()
                    .filter(|dep| self.nodes.iter().any(|other| &other.id == *dep))
                    .count();
                (node.id.as_str(), count)
            })
            .collect();
        let mut dependents: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for node in &self.nodes {
            for dep in &node.after {
                dependents.entry(dep.as_str()).or_default().push(&node.id);
            }
        }
        let mut queue: Vec<&str> = remaining_deps
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::new();
        while let Some(id) = queue.pop() {
            order.push(id);
            if let Some(children) = dependents.get(id) {
                for child in children {
                    if let Some(degree) = remaining_deps.get_mut(child) {
                        *degree = degree.saturating_sub(1);
                        if *degree == 0 {
                            queue.push(child);
                        }
                    }
                }
            }
        }
        if order.len() == self.nodes.len() {
            return None;
        }
        // Some cycle exists; find one by walking dependents from any
        // unprocessed node until a repeat.
        let remaining: Vec<&str> = self
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .filter(|id| !order.contains(id))
            .collect();
        let start = remaining[0];
        let mut path = vec![start.to_string()];
        let mut current = start;
        loop {
            let next = dependents
                .get(current)
                .and_then(|children| {
                    children
                        .iter()
                        .find(|child| !order.contains(child))
                        .copied()
                })
                .unwrap_or(start);
            if path.contains(&next.to_string()) {
                path.push(next.to_string());
                break;
            }
            path.push(next.to_string());
            current = next;
        }
        // Trim to the actual cycle (from first occurrence of the repeat).
        let repeat = path.last().unwrap().clone();
        if let Some(position) = path.iter().position(|id| *id == repeat) {
            path.drain(..position);
        }
        path.pop();
        Some(path)
    }

    /// Transitive dependency set of `id` (excluding itself unless cyclic).
    pub(crate) fn transitive_deps(&self, id: &str) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<&str> = self
            .node(id)
            .map(|node| node.after.iter().map(|dep| dep.as_str()).collect())
            .unwrap_or_default();
        while let Some(next) = stack.pop() {
            if seen.insert(next.to_string()) {
                if let Some(node) = self.node(next) {
                    for dep in &node.after {
                        stack.push(dep);
                    }
                }
            }
        }
        seen.into_iter().collect()
    }
}

/// `{{node_id.output}}` references inside an agent prompt, template, or
/// custom command.
pub(crate) fn template_references(node: &WorkflowNode) -> Vec<String> {
    let mut references = Vec::new();
    for text in [
        node.prompt.as_deref().unwrap_or(""),
        node.template.as_deref().unwrap_or(""),
        node.custom_command.as_deref().unwrap_or(""),
    ] {
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let after_braces = &rest[start + 2..];
            let Some(end) = after_braces.find("}}") else {
                break;
            };
            let token = &after_braces[..end];
            if let Some(id) = token.strip_suffix(".output") {
                if !id.is_empty()
                    && id
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_')
                {
                    references.push(id.to_string());
                }
            }
            rest = &after_braces[end + 2..];
        }
    }
    references.sort();
    references.dedup();
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_json(id: &str, node_type: &str) -> String {
        format!(r#"{{"id": "{id}", "type": "{node_type}"}}"#)
    }

    #[test]
    fn parses_minimal_workflow() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [{"id": "a", "type": "agent", "runtime": "claude-code", "prompt": "hi"}, {"id": "b", "type": "prompt_template", "template": "x"}]}"#,
        )
        .unwrap();
        assert_eq!(def.nodes.len(), 2);
        assert!(def.nodes[0].visible);
        assert_eq!(def.nodes[0].timeout_ms, 0);
    }

    #[test]
    fn rejects_duplicate_ids_and_unknown_deps() {
        let errors = WorkflowDef {
            name: "x".into(),
            nodes: vec![
                serde_json::from_str(&node_json("a", "prompt_template")).unwrap(),
                serde_json::from_str(&node_json("a", "prompt_template")).unwrap(),
                serde_json::from_str(
                    &(node_json("b", "prompt_template")
                        .trim_end_matches('}')
                        .to_string()
                        + r#", "after": ["ghost"]}"#),
                )
                .unwrap(),
            ],
        }
        .validate()
        .unwrap_err();
        assert!(errors.iter().any(|error| error.contains("unique")));
        assert!(errors
            .iter()
            .any(|error| error.contains("unknown dependency 'ghost'")));
    }

    #[test]
    fn rejects_cycles_with_path() {
        let errors = WorkflowDef {
            name: "x".into(),
            nodes: vec![
                serde_json::from_str(
                    &(node_json("a", "prompt_template")
                        .trim_end_matches('}')
                        .to_string()
                        + r#", "template": "t", "after": ["c"]}"#),
                )
                .unwrap(),
                serde_json::from_str(
                    &(node_json("b", "prompt_template")
                        .trim_end_matches('}')
                        .to_string()
                        + r#", "template": "t", "after": ["a"]}"#),
                )
                .unwrap(),
                serde_json::from_str(
                    &(node_json("c", "prompt_template")
                        .trim_end_matches('}')
                        .to_string()
                        + r#", "template": "t", "after": ["b"]}"#),
                )
                .unwrap(),
            ],
        }
        .validate()
        .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.contains("cycle") && error.contains("->")));
    }

    #[test]
    fn agent_nodes_need_prompt_and_runtime() {
        let errors = WorkflowDef {
            name: "x".into(),
            nodes: vec![serde_json::from_str(&node_json("a", "agent")).unwrap()],
        }
        .validate()
        .unwrap_err();
        assert!(errors.iter().any(|error| error.contains("prompt")));
        assert!(errors.iter().any(|error| error.contains("runtime")));
    }

    #[test]
    fn custom_agent_needs_command_not_prompt() {
        WorkflowDef {
            name: "x".into(),
            nodes: vec![serde_json::from_str(
                &(node_json("c", "agent").trim_end_matches('}').to_string()
                    + r#", "runtime": "custom", "custom_command": "echo {{prompt}}", "prompt": "the task"}"#),
            )
            .unwrap()],
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn template_reference_must_be_upstream() {
        let errors = WorkflowDef {
            name: "x".into(),
            nodes: vec![
                serde_json::from_str(
                    &(node_json("a", "agent").trim_end_matches('}').to_string()
                        + r#", "runtime": "claude-code", "prompt": "uses {{b.output}}"}"#),
                )
                .unwrap(),
                serde_json::from_str(
                    &(node_json("b", "prompt_template")
                        .trim_end_matches('}')
                        .to_string()
                        + r#", "template": "later"}"#),
                )
                .unwrap(),
            ],
        }
        .validate()
        .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.contains("does not depend on 'b'")));

        // Adding the dependency makes it valid.
        WorkflowDef {
            name: "x".into(),
            nodes: vec![
                serde_json::from_str(
                    &(node_json("a", "agent").trim_end_matches('}').to_string()
                        + r#", "runtime": "claude-code", "prompt": "uses {{b.output}}", "after": ["b"]}"#),
                )
                .unwrap(),
                serde_json::from_str(
                    &(node_json("b", "prompt_template").trim_end_matches('}').to_string()
                        + r#", "template": "earlier"}"#),
                )
                .unwrap(),
            ],
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn custom_prompt_placeholder_requires_prompt_field() {
        let errors = WorkflowDef {
            name: "x".into(),
            nodes: vec![serde_json::from_str(
                &(node_json("c", "agent").trim_end_matches('}').to_string()
                    + r#", "runtime": "custom", "custom_command": "echo {{prompt}}"}"#),
            )
            .unwrap()],
        }
        .validate()
        .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.contains("uses {{prompt}} but has no prompt")));
    }

    #[test]
    fn cache_projection_drops_presentation_fields() {
        let node: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "runtime": "claude-code", "prompt": "p", "title": "t", "position": {"x": 1, "y": 2}, "visible": false, "timeout_ms": 5, "retry": 2}"#),
        )
        .unwrap();
        let projection = node.cache_projection().to_string();
        assert!(projection.contains("claude-code"));
        assert!(projection.contains("\"prompt\""));
        assert!(!projection.contains("position"));
        assert!(!projection.contains("title"));
        assert!(!projection.contains("timeout_ms"));
    }

    #[test]
    fn extracts_template_references() {
        let node: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "prompt": "{{x.output}} then {{y.output}} and {{x.output}} not {{z.model}}"}"#),
        )
        .unwrap();
        assert_eq!(template_references(&node), vec!["x", "y"]);
    }

    #[test]
    fn parses_grok_and_dsh_runtimes() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "g", "type": "agent", "runtime": "grok-build", "prompt": "hi"},
                {"id": "d", "type": "agent", "runtime": "dsh", "prompt": "hi"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(def.nodes[0].runtime, Some(AgentRuntime::GrokBuild));
        assert_eq!(def.nodes[1].runtime, Some(AgentRuntime::Dsh));
    }

    #[test]
    fn enabled_defaults_true_and_participates_in_cache_hash() {
        let base: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "runtime": "claude-code", "prompt": "p"}"#),
        )
        .unwrap();
        assert!(base.enabled);
        let disabled: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "runtime": "claude-code", "prompt": "p", "enabled": false}"#),
        )
        .unwrap();
        assert!(!disabled.enabled);
        assert_ne!(
            base.cache_projection(),
            disabled.cache_projection(),
            "toggling enabled must invalidate the node cache key"
        );
    }

    #[test]
    fn dsh_permission_env_mapping() {
        assert_eq!(PermissionLevel::Readonly.dsh_env(), Some("read-only"));
        assert_eq!(PermissionLevel::Workspace.dsh_env(), None);
        assert_eq!(PermissionLevel::Full.dsh_env(), Some("danger-full-access"));
    }

    #[test]
    fn skip_on_error_defaults_false_and_participates_in_cache_hash() {
        let base: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "runtime": "claude-code", "prompt": "p"}"#),
        )
        .unwrap();
        assert!(!base.skip_on_error);
        let tolerant: WorkflowNode = serde_json::from_str(
            &(node_json("a", "agent").trim_end_matches('}').to_string()
                + r#", "runtime": "claude-code", "prompt": "p", "skip_on_error": true}"#),
        )
        .unwrap();
        assert!(tolerant.skip_on_error);
        assert_ne!(
            base.cache_projection(),
            tolerant.cache_projection(),
            "toggling skip_on_error must invalidate the node cache key"
        );
        // A false value serializes away entirely: the projection (and thus
        // the cache hash) is byte-identical to a pre-P3a node, so existing
        // on-disk caches survive the upgrade.
        let projection = base.cache_projection().to_string();
        assert!(
            !projection.contains("skip_on_error"),
            "false must not leak into the cache projection: {projection}"
        );
    }
}
