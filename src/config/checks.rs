//! Checks `glidesh validate` runs on a resolved plan without contacting any host.

use crate::config::types::{ParamValue, Plan};
use std::path::Path;

/// Local `file` sources that do not exist, one message each.
///
/// Resolved against `plan_dir` — the top-level plan's directory — exactly as the `file`
/// module resolves them at run time, included steps too. Skipped: `fetch` tasks, whose `src`
/// is a path on the host, and sources containing `${…}`, which only a run can resolve.
pub fn missing_file_sources(plan: &Plan, plan_dir: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for step in plan.steps() {
        for task in step.tasks.iter().filter(|t| t.module == "file") {
            let fetch = matches!(task.args.get("fetch"), Some(ParamValue::Bool(true)));
            let Some(src) = task.args.get("src").and_then(ParamValue::as_str) else {
                continue;
            };
            if fetch || src.contains("${") {
                continue;
            }
            let path = Path::new(src);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                plan_dir.join(path)
            };
            if !resolved.exists() {
                missing.push(format!(
                    "step '{}': file '{}': src '{}' not found (looked for {})",
                    step.name,
                    task.resource,
                    src,
                    resolved.display()
                ));
            }
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_plan;

    fn plan(body: &str) -> Plan {
        parse_plan(&format!("plan \"p\" {{\n{body}\n}}")).unwrap()
    }

    #[test]
    fn a_missing_source_is_reported_with_where_it_was_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(r#"step "Deploy" { file "/etc/app.conf" src="app.conf" }"#);
        let missing = missing_file_sources(&p, dir.path());
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("step 'Deploy'"), "{}", missing[0]);
        assert!(missing[0].contains("src 'app.conf'"), "{}", missing[0]);
        assert!(
            missing[0].contains(&dir.path().join("app.conf").display().to_string()),
            "{}",
            missing[0]
        );
    }

    #[test]
    fn an_existing_source_or_directory_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.conf"), "x").unwrap();
        std::fs::create_dir(dir.path().join("site")).unwrap();
        let p = plan(
            r#"step "Deploy" {
                file "/etc/app.conf" src="app.conf"
                file "/var/www" src="site" recurse=#true
            }"#,
        );
        assert!(missing_file_sources(&p, dir.path()).is_empty());
    }

    /// A fetch's `src` is on the host, and an interpolated one is unknown until the run.
    #[test]
    fn remote_and_interpolated_sources_are_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(
            r#"step "s" {
                file "backups/db.sql" src="/var/backups/db.sql" fetch=#true
                file "/etc/app.conf" src="${@host.name}.conf"
            }"#,
        );
        assert!(missing_file_sources(&p, dir.path()).is_empty());
    }
}
