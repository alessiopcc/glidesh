pub mod inventory;
pub mod plan;
pub mod template;
pub mod types;

pub use inventory::parse_inventory;
pub use plan::{parse_plan, resolve_includes};

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
