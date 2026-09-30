//! Checks `glidesh validate` runs on a resolved plan without contacting any host.

use crate::config::plan::{is_error_var, is_item_var};
use crate::config::template::{
    Token, bound_field, defined_references, literal_hint, structure_error, tokens,
};
use crate::config::types::{LoopSource, ParamValue, Plan, TaskDef};
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
                let shown = shown_source(&src, &resolved, &file);
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

/// What `validate` finds in templated `file` sources.
#[derive(Debug, Default)]
pub struct TemplateFindings {
    /// What fails the upload whatever the variables: a template `render` cannot read, a
    /// loop over a list nothing defines.
    pub problems: Vec<String>,
    /// `${name}` references nothing defines — unless an inventory `validate` was not
    /// given does.
    pub undefined: Vec<String>,
    /// `${name}` references only some of the plan's hosts define.
    pub partial: Vec<String>,
    /// `$${name}` whose `name` is defined: see [`old_escape_warning`].
    pub warnings: Vec<String>,
}

/// Which of the hosts a plan may run on define a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    All,
    /// Defined for some: these hosts lack it.
    Partial(Vec<String>),
    None,
}

/// The names the plan's tasks register, which later tasks read like any variable.
fn registered(plan: &Plan) -> std::collections::HashSet<&str> {
    plan.steps()
        .iter()
        .flat_map(|step| step.all_tasks())
        .filter_map(|task| task.register.as_deref())
        .collect()
}

/// `$${name}` whose `name` is defined: before `$${` was an escape, it meant a `$` and then
/// the value, and now writes `${name}` as it is.
fn old_escape_warning(at: &str, name: &str) -> String {
    format!(
        "{at}: $${{{name}}} writes a literal ${{{name}}}; for a `$` followed by the value of \
         '{name}' (what it meant before `$${{` was an escape), put the `$` into the value"
    )
}

/// Checks every local templated `file` source the way `render` will read it. The template
/// must be readable, each `${for}` must loop over a list `is_list` accepts, and each
/// `${name}` must be defined: `is_defined` (for every host), a name a task before this one
/// registers, the binding of a `${for}` around it, or a host variable — `hosts` tells which
/// hosts define it. Each name is reported once per file, at its first line.
pub fn template_reference_findings(
    plan: &Plan,
    plan_dir: &Path,
    is_defined: impl Fn(&str) -> bool,
    hosts: impl Fn(&str) -> Coverage,
    is_list: impl Fn(&str) -> bool,
) -> TemplateFindings {
    let mut registered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut findings = TemplateFindings::default();
    for step in plan.steps() {
        let item = match &step.loop_source {
            None => Item::NoLoop,
            Some(LoopSource::Variable(list)) if is_list(list) => Item::Fields,
            Some(_) => Item::Value,
        };
        // `rescue` and `always` run after the loop; `template_scope_problems` reports it.
        let blocks = [
            (&step.tasks, item),
            (&step.rescue, Item::Elsewhere),
            (&step.always, Item::Elsewhere),
        ];
        for (task, item) in blocks
            .into_iter()
            .flat_map(|(tasks, item)| tasks.iter().map(move |task| (task, item)))
        {
            if matches!(task.args.get("template"), Some(ParamValue::Bool(true))) {
                if let Some((src, resolved)) = local_source(task, step.base_dir(plan_dir)) {
                    let at = |shown: &str, line: usize| {
                        format!(
                            "step '{}': file '{}': template {}, line {}",
                            step.name, task.resource, shown, line
                        )
                    };
                    let defined = |name: &str| is_defined(name) || registered.contains(name);
                    let mut files = Vec::new();
                    if resolved.is_dir() {
                        files_under(&resolved, &mut files);
                    } else {
                        files.push(resolved.clone());
                    }
                    for file in files {
                        let shown = shown_source(&src, &resolved, &file);
                        // Missing files are `missing_file_sources`'.
                        let text = match std::fs::read(&file) {
                            Ok(bytes) => bytes,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(e) => {
                                findings.problems.push(format!(
                                    "step '{}': file '{}': template {}: cannot be read: {}",
                                    step.name, task.resource, shown, e
                                ));
                                continue;
                            }
                        };
                        let Ok(text) = String::from_utf8(text) else {
                            findings.problems.push(format!(
                                "step '{}': file '{}': template {} is not valid UTF-8, so it \
                                 cannot be rendered; upload it without `template #true`",
                                step.name, task.resource, shown
                            ));
                            continue;
                        };
                        check_template(
                            &text,
                            &|line| at(&shown, line),
                            &defined,
                            &hosts,
                            &is_list,
                            item,
                            &mut findings,
                        );
                    }
                }
            }
            if let Some(name) = task.register.as_deref() {
                registered.insert(name);
            }
        }
    }
    findings
}

/// What `${@item…}` a task's template can read: a step's own tasks get the loop's item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// The step has no `loop=`.
    NoLoop,
    /// A loop over lines or literal items: `${@item}`.
    Value,
    /// A loop over a list variable: `${@item.<field>}`.
    Fields,
    /// A `rescue` or `always` task, checked by [`template_scope_problems`].
    Elsewhere,
}

/// Why `name`, an `@item` reference, is not what `item` provides, or `None` when it is.
fn item_problem(name: &str, item: Item) -> Option<&'static str> {
    let fields = name.starts_with("@item.");
    match item {
        Item::Elsewhere => None,
        Item::NoLoop => Some("the step has no `loop=`"),
        Item::Value if fields => Some("the step loops over lines or items, read as ${@item}"),
        Item::Fields if !fields => {
            Some("the step loops over a list, whose fields are read as ${@item.<field>}")
        }
        Item::Value | Item::Fields => None,
    }
}

fn check_template(
    text: &str,
    at: &dyn Fn(usize) -> String,
    defined: &dyn Fn(&str) -> bool,
    hosts: &dyn Fn(&str) -> Coverage,
    is_list: &dyn Fn(&str) -> bool,
    item: Item,
    findings: &mut TemplateFindings,
) {
    if let Some((line, message)) = structure_error(text) {
        findings.problems.push(format!("{}: {}", at(line), message));
        return;
    }
    let mut seen_names = std::collections::HashSet::new();
    let mut seen_lists = std::collections::HashSet::new();
    let mut seen_escapes = std::collections::HashSet::new();
    let mut bindings: Vec<&str> = Vec::new();
    for (line, token) in tokens(text) {
        match token {
            Token::For {
                binding,
                collection,
                ..
            } => {
                if !is_list(collection) && seen_lists.insert(collection) {
                    findings.problems.push(format!(
                        "{}: loops over '{}', which is not a defined list",
                        at(line),
                        collection
                    ));
                }
                bindings.push(binding);
            }
            Token::EndFor => {
                bindings.pop();
            }
            Token::Var(name) => {
                let bound = bindings
                    .iter()
                    .any(|binding| bound_field(name, binding).is_some());
                if bound || !seen_names.insert(name) {
                    continue;
                }
                if name == "@item" || name.starts_with("@item.") {
                    if let Some(why) = item_problem(name, item) {
                        findings.problems.push(format!(
                            "{}: ${{{}}} is not defined here: {}",
                            at(line),
                            name,
                            why
                        ));
                    }
                    continue;
                }
                if defined(name) {
                    continue;
                }
                match hosts(name) {
                    Coverage::All => {}
                    Coverage::Partial(missing) => findings.partial.push(format!(
                        "{}: ${{{}}} is not set for host{} {}",
                        at(line),
                        name,
                        if missing.len() == 1 { "" } else { "s" },
                        missing.join(", ")
                    )),
                    Coverage::None => findings.undefined.push(format!(
                        "{}: ${{{}}} is not defined{}",
                        at(line),
                        name,
                        literal_hint(name)
                    )),
                }
            }
            Token::Escaped(name) => {
                if (defined(name) || hosts(name) != Coverage::None) && seen_escapes.insert(name) {
                    findings.warnings.push(old_escape_warning(&at(line), name));
                }
            }
            Token::Invalid(_) => {}
        }
    }
}

/// [`old_escape_warning`]s for the `$${name}` in the plan itself: task resources and
/// parameters, and `until=` commands.
pub fn escaped_parameter_warnings(plan: &Plan, is_defined: impl Fn(&str) -> bool) -> Vec<String> {
    let registered = registered(plan);
    let defined = |name: &str| is_defined(name) || registered.contains(name);
    let mut warnings = Vec::new();
    let mut check = |at: &str, text: &str| {
        for (_, token) in tokens(text) {
            if let Token::Escaped(name) = token {
                if defined(name) {
                    warnings.push(old_escape_warning(at, name));
                }
            }
        }
    };
    for step in plan.steps() {
        if let Some(until) = &step.until {
            check(&format!("step '{}': until", step.name), &until.command);
        }
        for task in step.all_tasks() {
            let at = format!("step '{}': {} '{}'", step.name, task.module, task.resource);
            check(&at, &task.resource);
            for value in task.args.values() {
                match value {
                    ParamValue::String(text) => check(&at, text),
                    ParamValue::List(items) => items.iter().for_each(|text| check(&at, text)),
                    ParamValue::Map(map) => map.values().for_each(|text| check(&at, text)),
                    _ => {}
                }
            }
        }
    }
    warnings
}

/// `file` as the plan names it: `src`, or `src/<path>` for one inside a recursive copy.
fn shown_source(src: &str, resolved: &Path, file: &Path) -> String {
    match file.strip_prefix(resolved) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!(
            "{}/{}",
            src.trim_end_matches('/'),
            rel.to_string_lossy().replace('\\', "/")
        ),
        _ => src.to_string(),
    }
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

    fn template_findings(template: &str, body: &str) -> TemplateFindings {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.conf"), template).unwrap();
        let p = plan(body);
        template_reference_findings(
            &p,
            dir.path(),
            |name| ["port", "db-host"].contains(&name) || name.starts_with("@host."),
            |name| match name {
                "region" => Coverage::All,
                "tier" => Coverage::Partial(vec!["db-1".to_string(), "db-2".to_string()]),
                _ => Coverage::None,
            },
            |list| ["api-keys", "hosts", "@group.web"].contains(&list),
        )
    }

    const TEMPLATED: &str =
        r#"step "Deploy" { file "/etc/app.conf" src="app.conf" template=#true }"#;

    #[test]
    fn a_defined_reference_passes_and_an_undefined_one_is_a_problem() {
        let found = template_findings(
            "port=${port}\nhost=${@host.name}\nhome=${HOME}\nuser=${user}\nagain=${HOME}\n",
            TEMPLATED,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.undefined.len(), 2, "{:?}", found.undefined);
        assert!(
            found.undefined[0].contains(
                "step 'Deploy': file '/etc/app.conf': template app.conf, line 3: ${HOME} is not defined"
            ),
            "{}",
            found.undefined[0]
        );
        assert!(
            found.undefined[0].contains("write $${HOME}"),
            "{}",
            found.undefined[0]
        );
        assert!(found.undefined[1].contains("line 4: ${user} is not defined"));
        assert!(
            !found.undefined[1].contains("$${"),
            "{}",
            found.undefined[1]
        );
        assert!(found.warnings.is_empty());
    }

    #[test]
    fn an_escaped_reference_is_text() {
        let found = template_findings("echo $${HOME} $${UNDEFINED:-x}\n", TEMPLATED);
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.undefined.is_empty(), "{:?}", found.undefined);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn a_loop_binding_is_defined_inside_its_loop_only() {
        let found = template_findings(
            "${for k in api-keys}${k.name}=${k.value}\n${endfor}${k.name}\n\
             ${for h in @group.web}${h.address}${endfor}\n${for x in missing}${endfor}",
            TEMPLATED,
        );
        assert_eq!(found.undefined.len(), 1, "{:?}", found.undefined);
        assert!(found.undefined[0].contains("line 2: ${k.name} is not defined"));
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("line 4: loops over 'missing', which is not a defined list"),
            "{}",
            found.problems[0]
        );
    }

    #[test]
    fn a_name_an_earlier_task_registers_is_defined_and_a_later_one_not() {
        let found = template_findings(
            "${disks} ${later}\n",
            &format!(
                r#"step "List" {{ shell "lsblk" register="disks" }}
                {TEMPLATED}
                step "After" {{ shell "true" register="later" }}"#
            ),
        );
        assert_eq!(found.undefined.len(), 1, "{:?}", found.undefined);
        assert!(found.undefined[0].contains("${later} is not defined"));
    }

    #[test]
    fn an_escape_of_a_defined_name_warns_it_changed_meaning() {
        let found = template_findings("price=$${port}\n", TEMPLATED);
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1);
        assert!(
            found.warnings[0].contains("line 1: $${port} writes a literal ${port}"),
            "{}",
            found.warnings[0]
        );
    }

    #[test]
    fn an_untemplated_or_interpolated_source_is_not_checked() {
        let untemplated = template_findings(
            "${HOME}",
            r#"step "s" { file "/etc/app.conf" src="app.conf" }"#,
        );
        assert!(untemplated.undefined.is_empty());
        let interpolated = template_findings(
            "${HOME}",
            r#"step "s" { file "/etc/app.conf" src="${name}.conf" template=#true }"#,
        );
        assert!(interpolated.undefined.is_empty());
    }

    #[test]
    fn a_template_render_cannot_read_is_a_problem_at_its_line() {
        for (template, expected) in [
            (
                "ok\n${for x of api-keys}${endfor}",
                "line 2: Invalid for-loop syntax",
            ),
            (
                "${for @k in api-keys}${endfor}",
                "line 1: for-loop binding '@k' cannot use the reserved",
            ),
            (
                "a\n\n${for k in api-keys}${k.name}",
                "line 3: Missing ${endfor} for loop over 'api-keys'",
            ),
            ("port=${port\n", "line 1: Unclosed variable reference"),
        ] {
            let found = template_findings(template, TEMPLATED);
            assert_eq!(
                found.problems.len(),
                1,
                "{template:?}: {:?}",
                found.problems
            );
            assert!(
                found.problems[0].contains(expected),
                "{template:?}: {}",
                found.problems[0]
            );
        }
    }

    #[test]
    fn a_loop_over_an_unknown_group_is_a_problem() {
        let found = template_findings(
            "${for h in @group.web}${endfor}${for h in @group.wbe}${endfor}",
            TEMPLATED,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(found.problems[0].contains("loops over '@group.wbe'"));
    }

    #[test]
    fn a_dotted_binding_is_matched_whole() {
        let found = template_findings(
            "${for host.item in hosts}${host.item.name}${endfor}${host.name}",
            TEMPLATED,
        );
        assert_eq!(found.undefined.len(), 1, "{:?}", found.undefined);
        assert!(found.undefined[0].contains("${host.name} is not defined"));
    }

    #[test]
    fn a_host_variable_some_hosts_lack_is_partial() {
        let found = template_findings("${region} ${tier}\n", TEMPLATED);
        assert!(found.undefined.is_empty(), "{:?}", found.undefined);
        assert_eq!(found.partial.len(), 1, "{:?}", found.partial);
        assert!(
            found.partial[0].contains("line 1: ${tier} is not set for hosts db-1, db-2"),
            "{}",
            found.partial[0]
        );
    }

    #[test]
    fn a_list_and_a_variable_of_one_name_are_both_reported() {
        let found = template_findings("${for x in logs}${endfor}${logs}\n", TEMPLATED);
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert_eq!(found.undefined.len(), 1, "{:?}", found.undefined);
    }

    #[test]
    fn an_escape_of_a_defined_name_in_a_parameter_warns_too() {
        let p = plan(
            r#"step "s" until="test -n $${port}" {
                shell "echo $${port} $${HOME}" register="out"
                shell "echo $${out}" { environment { COST "$${port}" } }
            }"#,
        );
        let warnings = escaped_parameter_warnings(&p, |name| name == "port");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(
            warnings[0].starts_with("step 's': until: $${port}"),
            "{}",
            warnings[0]
        );
        assert!(
            warnings[1].starts_with("step 's': shell 'echo $${port} $${HOME}': $${port}"),
            "{}",
            warnings[1]
        );
        assert!(
            warnings.iter().any(|w| w.contains("$${out}")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_item_reference_follows_the_steps_loop() {
        let file = r#"file "/etc/app.conf" src="app.conf" template=#true"#;
        let cases = [
            (
                format!(r#"step "s" {{ {file} }}"#),
                "${@item}",
                Some("has no `loop=`"),
            ),
            (
                format!(r#"step "s" loop="${{port}}" {{ {file} }}"#),
                "${@item}",
                None,
            ),
            (
                format!(r#"step "s" loop="${{port}}" {{ {file} }}"#),
                "${@item.name}",
                Some("loops over lines or items"),
            ),
            (
                format!(r#"step "s" loop="${{api-keys}}" {{ {file} }}"#),
                "${@item.name}",
                None,
            ),
            (
                format!(r#"step "s" loop="${{api-keys}}" {{ {file} }}"#),
                "${@item}",
                Some("loops over a list"),
            ),
            (
                format!(r#"step "s" {{ shell "true"; rescue {{ {file} }} }}"#),
                "${@item}",
                None,
            ),
        ];
        for (body, template, expected) in cases {
            let found = template_findings(template, &body);
            match expected {
                None => assert!(
                    found.problems.is_empty(),
                    "{body} {template}: {:?}",
                    found.problems
                ),
                Some(why) => {
                    assert_eq!(
                        found.problems.len(),
                        1,
                        "{body} {template}: {:?}",
                        found.problems
                    );
                    assert!(found.problems[0].contains(why), "{}", found.problems[0]);
                }
            }
        }
    }

    #[test]
    fn a_template_that_is_not_utf8_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.conf"), b"\xff\xfe").unwrap();
        let p = plan(TEMPLATED);
        let found =
            template_reference_findings(&p, dir.path(), |_| true, |_| Coverage::All, |_| true);
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("is not valid UTF-8"),
            "{}",
            found.problems[0]
        );
    }
}
