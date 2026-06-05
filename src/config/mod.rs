pub mod inventory;
pub mod plan;
pub mod template;
pub mod types;

pub use inventory::parse_inventory;
pub use plan::{parse_plan, resolve_includes};

use crate::config::types::{RunAsMethod, RunAsSpec, RunAsUser};
use crate::error::GlideshError;

/// Read `run-as` / `run-as-method` attributes from a node (host, group, step, or task).
/// Absent `run-as` => inherit; `run-as=""` (or null) => explicitly no escalation.
pub(crate) fn parse_run_as_attrs(node: &kdl::KdlNode) -> Result<RunAsSpec, GlideshError> {
    let user = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("run-as"))
        .map(|e| run_as_user_from_value(e.value()));
    Ok(RunAsSpec {
        user,
        method: parse_run_as_method_attr(node)?,
    })
}

/// `run-as="x"` => escalate to x; `run-as=""` (or null) => explicitly no escalation.
pub(crate) fn run_as_user_from_value(value: &kdl::KdlValue) -> RunAsUser {
    match value.as_string() {
        Some(s) if !s.is_empty() => RunAsUser::User(s.to_string()),
        _ => RunAsUser::Disabled,
    }
}

pub(crate) fn parse_run_as_method_attr(
    node: &kdl::KdlNode,
) -> Result<Option<RunAsMethod>, GlideshError> {
    match node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("run-as-method"))
    {
        Some(e) => {
            let s = e
                .value()
                .as_string()
                .ok_or_else(|| GlideshError::ConfigParse {
                    message: "run-as-method must be a string".to_string(),
                })?;
            let method = RunAsMethod::parse(s).ok_or_else(|| GlideshError::ConfigParse {
                message: format!("Unknown run-as-method '{}' (expected sudo, doas, or su)", s),
            })?;
            Ok(Some(method))
        }
        None => Ok(None),
    }
}

/// Format a KDL parse error with line/column and a source snippet.
fn format_kdl_error(input: &str, err: &kdl::KdlError) -> String {
    let mut parts = Vec::new();
    for diag in &err.diagnostics {
        let offset = diag.span.offset();
        let (line, col) = offset_to_line_col(input, offset);
        let msg = diag
            .message
            .as_deref()
            .or(diag.label.as_deref())
            .unwrap_or("parse error");

        let source_line = input.lines().nth(line.saturating_sub(1)).unwrap_or("");
        parts.push(format!(
            "  line {}, col {}: {}\n  | {}\n  | {}^",
            line,
            col,
            msg,
            source_line,
            " ".repeat(col.saturating_sub(1)),
        ));

        if let Some(ref help) = diag.help {
            parts.push(format!("  help: {}", help));
        }
    }
    if parts.is_empty() {
        err.to_string()
    } else {
        parts.join("\n")
    }
}

fn offset_to_line_col(input: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in input.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
