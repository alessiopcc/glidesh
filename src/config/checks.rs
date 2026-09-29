//! Checks `glidesh validate` runs on a resolved plan without contacting any host.

use crate::config::plan::{is_error_var, is_item_var};
use crate::config::template::defined_references;
use crate::config::types::{ParamValue, Plan, TaskDef};
use std::path::{Path, PathBuf};

/// A task's local `file` source, resolved as a run resolves it, or `None` for a task with
/// no local source to inspect: not a `file` task, a `fetch` (whose `src` is on the host), or
/// a `src` containing `${…}`, which only a run can resolve.
fn local_source(task: &TaskDef, plan_dir: &Path) -> Option<(String, PathBuf)> {
    if task.module != "file" || matches!(task.args.get("fetch"), Some(ParamValue::Bool(true))) {
        return None;
    }
    let src = task.args.get("src").and_then(ParamValue::as_str)?;
    if src.contains("${") {
        return None;
    }
    let path = Path::new(src);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        plan_dir.join(path)
    };
    Some((src.to_string(), resolved))
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Warnings for `file` uploads without `template #true` whose content contains `${name}`
/// references that `is_defined` accepts — they would ship verbatim.
pub fn literal_reference_warnings(
    plan: &Plan,
    plan_dir: &Path,
    is_defined: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for step in plan.steps() {
        for task in step.all_tasks() {
            let templated = matches!(task.args.get("template"), Some(ParamValue::Bool(true)));
            let Some((src, resolved)) =
                local_source(task, step.base_dir(plan_dir)).filter(|_| !templated)
            else {
                continue;
            };
            let mut files = Vec::new();
            if resolved.is_dir() {
                files_under(&resolved, &mut files);
            } else {
                files.push(resolved.clone());
            }
            for file in files {
                let Ok(content) = std::fs::read(&file) else {
                    continue;
                };
                let names = defined_references(&content, &is_defined);
                if names.is_empty() {
                    continue;
                }
                let shown = match file.strip_prefix(&resolved) {
                    Ok(rel) if !rel.as_os_str().is_empty() => format!(
                        "{}/{}",
                        src.trim_end_matches('/'),
                        rel.to_string_lossy().replace('\\', "/")
                    ),
                    _ => src.clone(),
                };
                let refs: Vec<String> = names.iter().map(|n| format!("${{{n}}}")).collect();
                warnings.push(format!(
                    "step '{}': {} contains {} but is uploaded as-is, because `template` is not \
                     set; add `template #true` to substitute",
                    step.name,
                    shown,
                    refs.join(", ")
                ));
            }
        }
    }
    warnings
}

/// Tasks, the variables they cannot read, and why.
type Block<'a> = (&'a [TaskDef], fn(&str) -> bool, &'a str);

/// Variables a `file` template reads where they never exist: `${@error.*}` in a step's own
/// tasks, which run before any failure, and `${@item}` in its `rescue` and `always`, which run
/// after the loop. The parser rejects both in the plan; a template is only read here, and at
/// run time would fail its task on the undefined variable.
pub fn template_scope_problems(plan: &Plan, plan_dir: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for step in plan.steps() {
        let blocks: [Block; 3] = [
            (
                &step.tasks,
                is_error_var,
                "only rescue and always tasks can use it",
            ),
            (
                &step.rescue,
                is_item_var,
                "rescue runs once per step, after the loop",
            ),
            (
                &step.always,
                is_item_var,
                "always runs once per step, after the loop",
            ),
        ];
        for (tasks, out_of_scope, why) in blocks {
            for task in tasks {
                if !matches!(task.args.get("template"), Some(ParamValue::Bool(true))) {
                    continue;
                }
                let Some((src, resolved)) = local_source(task, step.base_dir(plan_dir)) else {
                    continue;
                };
                let mut files = Vec::new();
                if resolved.is_dir() {
                    files_under(&resolved, &mut files);
                } else {
                    files.push(resolved);
                }
                let names: std::collections::BTreeSet<String> = files
                    .iter()
                    .filter_map(|file| std::fs::read(file).ok())
                    .flat_map(|content| defined_references(&content, out_of_scope))
                    .collect();
                for name in names {
                    problems.push(format!(
                        "step '{}': file '{}': template {} uses ${{{}}}, which is never \
                         defined there: {}",
                        step.name, task.resource, src, name, why
                    ));
                }
            }
        }
    }
    problems
}

/// `file` sources that are not given, or given but not found locally, one message each.
///
/// Every `file` mode needs a string `src`, so a task without one would fail at run time.
/// Local sources are resolved exactly as the `file` module resolves them: from the directory
/// of the plan the step was written in, which is `plan_dir` for the top-level plan.
pub fn missing_file_sources(plan: &Plan, plan_dir: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for step in plan.steps() {
        for task in step.all_tasks() {
            if task.module == "file" && task.args.get("src").and_then(ParamValue::as_str).is_none()
            {
                missing.push(format!(
                    "step '{}': file '{}': src is required, as a string",
                    step.name, task.resource
                ));
                continue;
            }
            let Some((src, resolved)) = local_source(task, step.base_dir(plan_dir)) else {
                continue;
            };
            if resolved.exists() {
                continue;
            }
            let mut message = format!(
                "step '{}': file '{}': src '{}' not found (looked for {})",
                step.name,
                task.resource,
                src,
                resolved.display()
            );
            // Included plans used to resolve from the top-level plan's directory, so a plan
            // written against that finds its file there.
            if step.source_dir.is_some() {
                let old = plan_dir.join(&src);
                if old.exists() {
                    message.push_str(&format!(
                        "; {} exists, but an included plan's sources resolve from its own \
                         directory",
                        old.display()
                    ));
                }
            }
            missing.push(message);
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
    fn a_file_task_without_a_string_src_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(
            r#"step "s" {
                file "/etc/a.conf" mode="0644"
                file "/etc/b.conf" src=42
                file "backups/db.sql" fetch=#true
            }"#,
        );
        let missing = missing_file_sources(&p, dir.path());
        assert_eq!(missing.len(), 3, "{missing:?}");
        for (dest, msg) in ["/etc/a.conf", "/etc/b.conf", "backups/db.sql"]
            .iter()
            .zip(&missing)
        {
            assert!(
                msg.contains(dest) && msg.contains("src is required"),
                "{msg}"
            );
        }
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

    #[test]
    fn a_template_reading_a_variable_its_block_never_has_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("body.txt"), "${@error.msg} ${@item}").unwrap();
        std::fs::write(
            dir.path().join("section.txt"),
            "${@error.msg} ${@item.name}",
        )
        .unwrap();
        let p = plan(
            r#"step "s" loop="${xs}" {
                file "/a" src="body.txt" template=#true
                rescue { file "/b" src="section.txt" template=#true }
                always { file "/c" src="section.txt" template=#true }
            }"#,
        );
        let problems = template_scope_problems(&p, dir.path());
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].contains("file '/a': template body.txt uses ${@error.msg}")
                && problems[0].contains("only rescue and always"),
            "{}",
            problems[0]
        );
        assert!(
            problems[1].contains("file '/b': template section.txt uses ${@item.name}")
                && problems[1].contains("rescue runs once per step"),
            "{}",
            problems[1]
        );
        assert!(
            problems[2].contains("file '/c'") && problems[2].contains("always runs once"),
            "{}",
            problems[2]
        );
    }

    /// Uploaded as-is, the text is never rendered, so nothing in it is read.
    #[test]
    fn an_untemplated_file_is_not_scope_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("body.txt"), "${@error.msg}").unwrap();
        let p = plan(r#"step "s" { file "/a" src="body.txt" }"#);
        assert!(template_scope_problems(&p, dir.path()).is_empty());
    }

    fn warnings_for(p: &Plan, dir: &Path, defined: &[&str]) -> Vec<String> {
        literal_reference_warnings(p, dir, |n| defined.contains(&n))
    }

    #[test]
    fn an_untemplated_file_with_a_defined_reference_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vllm.env"),
            "CUDA_VISIBLE_DEVICES=${cuda-devices}\nPATH=${PATH}\n",
        )
        .unwrap();
        let p = plan(r#"step "Env" { file "/etc/vllm/vllm.env" src="vllm.env" }"#);
        let w = warnings_for(&p, dir.path(), &["cuda-devices"]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("step 'Env'"), "{}", w[0]);
        assert!(
            w[0].contains("vllm.env contains ${cuda-devices}"),
            "{}",
            w[0]
        );
        assert!(
            !w[0].contains("PATH"),
            "a shell variable is not glidesh's: {}",
            w[0]
        );
    }

    #[test]
    fn a_templated_file_does_not_warn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.conf"), "port=${port}").unwrap();
        let p = plan(r#"step "s" { file "/etc/app.conf" src="app.conf" template=#true }"#);
        assert!(warnings_for(&p, dir.path(), &["port"]).is_empty());
    }

    #[test]
    fn each_file_of_a_directory_upload_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("site/conf")).unwrap();
        std::fs::write(dir.path().join("site/index.html"), "plain").unwrap();
        std::fs::write(dir.path().join("site/conf/app.ini"), "port=${port}").unwrap();
        let p = plan(r#"step "s" { file "/var/www" src="site" recurse=#true }"#);
        let w = warnings_for(&p, dir.path(), &["port"]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("site/conf/app.ini contains ${port}"),
            "{}",
            w[0]
        );
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

    /// `main.kdl` includes `roles/web/plan.kdl`, whose step uploads `src="app.conf"`.
    fn included_upload(dir: &Path) -> Plan {
        std::fs::create_dir_all(dir.join("roles/web")).unwrap();
        std::fs::write(
            dir.join("roles/web/plan.kdl"),
            r#"plan "web" { step "Web" { file "/etc/app.conf" src="app.conf" } }"#,
        )
        .unwrap();
        let mut p = parse_plan(r#"plan "main" { include "roles/web/plan.kdl" }"#).unwrap();
        crate::config::resolve_includes(&mut p, dir).unwrap();
        p
    }

    #[test]
    fn an_included_plan_finds_its_sources_in_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = included_upload(dir.path());
        std::fs::write(dir.path().join("roles/web/app.conf"), "x").unwrap();
        assert!(missing_file_sources(&p, dir.path()).is_empty());
    }

    #[test]
    fn a_source_only_beside_the_top_level_plan_names_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = included_upload(dir.path());
        std::fs::write(dir.path().join("app.conf"), "x").unwrap();
        let missing = missing_file_sources(&p, dir.path());
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(
            missing[0].contains("roles") && missing[0].contains("resolve from its own directory"),
            "{}",
            missing[0]
        );
    }

    #[test]
    fn a_top_level_missing_source_has_no_include_hint() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(r#"step "Deploy" { file "/etc/app.conf" src="app.conf" }"#);
        let missing = missing_file_sources(&p, dir.path());
        assert!(!missing[0].contains("own directory"), "{}", missing[0]);
    }

    #[test]
    fn an_included_plan_warns_about_its_own_sources() {
        let dir = tempfile::tempdir().unwrap();
        let p = included_upload(dir.path());
        std::fs::write(dir.path().join("roles/web/app.conf"), "port=${port}").unwrap();
        assert_eq!(warnings_for(&p, dir.path(), &["port"]).len(), 1);
    }
}
