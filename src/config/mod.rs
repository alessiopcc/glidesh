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
        .map(|e| run_as_user_from_value(e.value()))
        .transpose()?;
    Ok(RunAsSpec {
        user,
        method: parse_run_as_method_attr(node)?,
    })
}

/// `run-as="x"` => escalate to x; `run-as=""` (or null) => explicitly no escalation.
/// A non-string, non-null value (e.g. `run-as=123`) is a typo, not a silent opt-out,
/// so it is rejected rather than disabling escalation unexpectedly.
pub(crate) fn run_as_user_from_value(value: &kdl::KdlValue) -> Result<RunAsUser, GlideshError> {
    match value {
        kdl::KdlValue::String(s) if !s.is_empty() => Ok(RunAsUser::User(s.clone())),
        kdl::KdlValue::String(_) | kdl::KdlValue::Null => Ok(RunAsUser::Disabled),
        _ => Err(GlideshError::ConfigParse {
            message: r#"run-as must be a string: a username, or "" to disable escalation"#
                .to_string(),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_as_value_string_is_user() {
        let v = kdl::KdlValue::String("root".to_string());
        assert_eq!(
            run_as_user_from_value(&v).unwrap(),
            RunAsUser::User("root".to_string())
        );
    }

    #[test]
    fn run_as_value_empty_and_null_disable() {
        assert_eq!(
            run_as_user_from_value(&kdl::KdlValue::String(String::new())).unwrap(),
            RunAsUser::Disabled
        );
        assert_eq!(
            run_as_user_from_value(&kdl::KdlValue::Null).unwrap(),
            RunAsUser::Disabled
        );
    }

    #[test]
    fn run_as_value_non_string_is_rejected() {
        // A typo like `run-as=123` must error, not silently disable escalation.
        assert!(run_as_user_from_value(&kdl::KdlValue::Integer(123)).is_err());
        assert!(run_as_user_from_value(&kdl::KdlValue::Bool(true)).is_err());
    }
}
