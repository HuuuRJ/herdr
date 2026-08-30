//! Workflow engine — `.aflow.json` DAG orchestration over agent panes.
//!
//! Layering: `model` parses and validates the workflow file, `engine` is the
//! pure per-run state machine (phases, readiness, completion), `runs` owns
//! the on-disk run store and content-hash cache keys, and `executors` build
//! the concrete commands (shell-wrapped panes, background processes, image
//! requests). `graph` is the pure DAG layout for the TUI graph view. The
//! App-facing glue lives in `crate::app::workflow`.

pub(crate) mod engine;
pub(crate) mod executors;
pub(crate) mod graph;
pub(crate) mod model;
pub(crate) mod runs;
