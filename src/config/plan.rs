use crate::config::condition::Condition;
use crate::config::types::{
    Amount, ExecutionMode, LoopSource, ParamValue, Plan, PlanItem, RunAsSpec, Step, TaskDef,
};
use crate::error::GlideshError;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn parse_plan(input: &str) -> Result<Plan, GlideshError> {
    let doc: kdl::KdlDocument = input.parse().map_err(|e: kdl::KdlError| {
        let details = super::format_kdl_error(input, &e);
        GlideshError::ConfigParse {
            message: format!("Failed to parse plan KDL:\n{}", details),
        }
    })?;

    let fp_node = doc
        .nodes()
        .iter()
        .find(|n| n.name().to_string() == "plan")
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "No 'plan' node found".to_string(),
        })?;

    let name = fp_node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "Plan requires a name argument".to_string(),
        })?
        .to_string();

    let children = fp_node
        .children()
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "Plan has no body".to_string(),
        })?;

    let mut mode = ExecutionMode::default();
    let mut serial = Vec::new();
    let mut max_fail = None;
    let mut vars = HashMap::new();
    let mut structured_vars: HashMap<String, Vec<HashMap<String, String>>> = HashMap::new();
    let mut vars_files = Vec::new();
    let mut items = Vec::new();

    for node in children.nodes() {
        match node.name().to_string().as_str() {
            "mode" => {
                let mode_str = node
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_string());
                mode = match mode_str {
                    Some("sync") => ExecutionMode::Sync,
                    Some("async") => ExecutionMode::Async,
                    // A typo used to fall back to sync silently.
                    other => {
                        return Err(GlideshError::ConfigParse {
                            message: format!(
                                "mode must be \"sync\" or \"async\", got {}",
                                other.map_or("nothing".to_string(), |m| format!("\"{m}\""))
                            ),
                        });
                    }
                };
            }
            "serial" => {
                serial = node
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .map(|e| parse_amount(e.value(), "serial", 1))
                    .collect::<Result<_, _>>()?;
                if serial.is_empty() {
                    return Err(GlideshError::ConfigParse {
                        message: "serial needs at least one batch size, e.g. serial 2 or \
                                  serial 1 \"25%\""
                            .to_string(),
                    });
                }
            }
            "max-fail" => {
                let mut values = node.entries().iter().filter(|e| e.name().is_none());
                let (Some(value), None) = (values.next(), values.next()) else {
                    return Err(GlideshError::ConfigParse {
                        message: "max-fail takes one value, e.g. max-fail 2 or \
                                  max-fail \"10%\""
                            .to_string(),
                    });
                };
                max_fail = Some(parse_amount(value.value(), "max-fail", 0)?);
            }
            "vars" => {
                if let Some(vc) = node.children() {
                    for vnode in vc.nodes() {
                        super::validate_user_var_name(vnode.name().value())?;
                        let key = vnode.name().to_string();
                        if let Some(list_of_maps) = super::parse_structured_var(vnode) {
                            if structured_vars.contains_key(&key) || vars.contains_key(&key) {
                                return Err(GlideshError::ConfigParse {
                                    message: format!(
                                        "Duplicate variable '{}' in plan vars block",
                                        key
                                    ),
                                });
                            }
                            structured_vars.insert(key, list_of_maps);
                        } else {
                            if vars.contains_key(&key) || structured_vars.contains_key(&key) {
                                return Err(GlideshError::ConfigParse {
                                    message: format!(
                                        "Duplicate variable '{}' in plan vars block",
                                        key
                                    ),
                                });
                            }
                            let value = vnode
                                .entries()
                                .iter()
                                .find(|e| e.name().is_none())
                                .map(|e| super::kdl_value_to_string(e.value()))
                                .unwrap_or_default();
                            vars.insert(key, value);
                        }
                    }
                }
            }
            "step" => {
                items.push(PlanItem::Step(parse_step(node)?));
            }
            "vars-file" => {
                let path = node
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| GlideshError::ConfigParse {
                        message: "vars-file requires a path argument".to_string(),
                    })?
                    .to_string();
                vars_files.push(path);
            }
            "include" => {
                let path = node
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| GlideshError::ConfigParse {
                        message: "include requires a path argument".to_string(),
                    })?
                    .to_string();
                items.push(PlanItem::Include(path));
            }
            other => {
                return Err(GlideshError::ConfigParse {
                    message: format!("Unknown node in plan: '{}'", other),
                });
            }
        }
    }

    let run_as = super::parse_run_as_attrs(fp_node)?;

    Ok(Plan {
        name,
        mode,
        serial,
        max_fail,
        vars,
        structured_vars,
        vars_files,
        run_as,
        items,
    })
}

/// A host count (`2`) or a percentage (`"25%"`), at least `min`. A count may also be written
/// as a string (`"2"`).
fn parse_amount(value: &kdl::KdlValue, setting: &str, min: usize) -> Result<Amount, GlideshError> {
    let fail = |got: String| GlideshError::ConfigParse {
        message: format!(
            "{setting} must be a host count of at least {min} or a percentage of \
             {min}–100%, got {got}"
        ),
    };
    if let Some(n) = value.as_integer() {
        return usize::try_from(n)
            .ok()
            .filter(|n| *n >= min)
            .map(Amount::Count)
            .ok_or_else(|| fail(n.to_string()));
    }
    let Some(text) = value.as_string() else {
        return Err(fail(value.to_string()));
    };
    let shown = format!("\"{text}\"");
    match text.trim().strip_suffix('%') {
        Some(pct) => pct
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= 100 && usize::from(*p) >= min)
            .map(Amount::Percent)
            .ok_or_else(|| fail(shown)),
        None => text
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= min)
            .map(Amount::Count)
            .ok_or_else(|| fail(shown)),
    }
}

/// Recursively resolve all `include` items in a plan by loading referenced plan files
/// and inlining their steps. The included plan's vars are merged (parent wins on conflict).
/// Also resolves `vars-file` directives by loading external KDL var files.
/// Detects circular includes.
pub fn resolve_includes(plan: &mut Plan, base_dir: &Path) -> Result<(), GlideshError> {
    resolve_vars_files(
        &plan.vars_files,
        &mut plan.vars,
        &mut plan.structured_vars,
        base_dir,
    )?;
    plan.vars_files.clear();

    let mut seen = HashSet::new();
    seen.insert(plan.name.clone());
    // The top-level plan's own run-as is applied at execution time (the `plan`
    // tier of the merge), so steps start with no inherited escalation here.
    // Included plans contribute their plan-level run-as to their own steps below.
    let resolved = resolve_items(
        &plan.items,
        &plan.vars,
        &plan.structured_vars,
        base_dir,
        &mut seen,
        &RunAsSpec::default(),
    )?;
    plan.items = resolved;

    // Validate step names are unique and subscribe references point to preceding steps
    let mut seen_steps: Vec<String> = Vec::new();
    for step in plan.steps() {
        if seen_steps.contains(&step.name) {
            return Err(GlideshError::ConfigParse {
                message: format!("Duplicate step name: '{}'", step.name),
            });
        }
        for sub in &step.subscribe {
            if !seen_steps.contains(sub) {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "Step '{}' subscribes to '{}', which is not a preceding step",
                        step.name, sub
                    ),
                });
            }
        }
        seen_steps.push(step.name.clone());
    }

    Ok(())
}

/// Load vars from external KDL files. Each file contains raw var nodes (no wrapper).
/// Inline vars take precedence over vars-file vars. Duplicate keys across different
/// vars-files are rejected.
fn resolve_vars_files(
    paths: &[String],
    vars: &mut HashMap<String, String>,
    structured_vars: &mut HashMap<String, Vec<HashMap<String, String>>>,
    base_dir: &Path,
) -> Result<(), GlideshError> {
    // Track keys seen across all vars-files to detect cross-file duplicates.
    // Keys already in inline vars/structured_vars are fine (inline wins).
    let mut seen_across_files: HashMap<String, String> = HashMap::new();

    for path in paths {
        let resolved_path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            base_dir.join(path)
        };
        let content = std::fs::read_to_string(&resolved_path).map_err(|e| {
            GlideshError::Other(format!(
                "Failed to read vars file '{}': {}",
                resolved_path.display(),
                e
            ))
        })?;
        let doc: kdl::KdlDocument =
            content
                .parse()
                .map_err(|e: kdl::KdlError| GlideshError::ConfigParse {
                    message: format!(
                        "Failed to parse vars file '{}': {}",
                        resolved_path.display(),
                        e
                    ),
                })?;
        let mut seen_in_file: HashSet<String> = HashSet::new();
        for vnode in doc.nodes() {
            super::validate_user_var_name(vnode.name().value())?;
            let key = vnode.name().to_string();
            if !seen_in_file.insert(key.clone()) {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "Duplicate variable '{}' in vars file '{}'",
                        key,
                        resolved_path.display()
                    ),
                });
            }
            // Check for duplicates across different vars-files
            if let Some(prev_file) = seen_across_files.get(&key) {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "Variable '{}' defined in both '{}' and '{}'",
                        key, prev_file, path
                    ),
                });
            }
            seen_across_files.insert(key.clone(), path.clone());
            if let Some(list_of_maps) = super::parse_structured_var(vnode) {
                // Inline structured vars win — only insert if not already present
                structured_vars.entry(key).or_insert(list_of_maps);
            } else {
                let value = vnode
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .map(|e| super::kdl_value_to_string(e.value()))
                    .unwrap_or_default();
                // Inline vars win — only insert if not already present
                vars.entry(key).or_insert(value);
            }
        }
    }
    Ok(())
}

fn resolve_items(
    items: &[PlanItem],
    parent_vars: &HashMap<String, String>,
    parent_structured: &HashMap<String, Vec<HashMap<String, String>>>,
    base_dir: &Path,
    seen: &mut HashSet<String>,
    inherited_run_as: &RunAsSpec,
) -> Result<Vec<PlanItem>, GlideshError> {
    let mut result = Vec::new();
    for item in items {
        match item {
            PlanItem::Step(s) => {
                // Flattening discards the nested-plan structure, so an including
                // plan's plan-level run-as is layered onto each inlined step here
                // (the step's own run-as still wins). Without this, escalation set
                // at the plan level of an included file would be lost.
                let mut s = s.clone();
                s.run_as = s.run_as.clone().merge_over(inherited_run_as);
                result.push(PlanItem::Step(s));
            }
            PlanItem::Include(path) => {
                let resolved_path = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    base_dir.join(path)
                };
                let content = std::fs::read_to_string(&resolved_path).map_err(|e| {
                    GlideshError::Other(format!(
                        "Failed to read included plan '{}': {}",
                        resolved_path.display(),
                        e
                    ))
                })?;
                let included = parse_plan(&content)?;
                if !seen.insert(included.name.clone()) {
                    return Err(GlideshError::ConfigParse {
                        message: format!(
                            "Circular include detected: plan '{}' already included",
                            included.name
                        ),
                    });
                }
                // Merge vars: included plan vars, then parent vars override
                let mut merged_vars = included.vars.clone();
                merged_vars.extend(parent_vars.iter().map(|(k, v)| (k.clone(), v.clone())));

                let mut merged_structured = included.structured_vars.clone();
                merged_structured.extend(
                    parent_structured
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );

                // The included plan's plan-level run-as governs its own steps,
                // layered under anything inherited from the including plan(s).
                let child_run_as = included.run_as.clone().merge_over(inherited_run_as);
                let child_base = resolved_path.parent().unwrap_or(base_dir);
                let child_items = resolve_items(
                    &included.items,
                    &merged_vars,
                    &merged_structured,
                    child_base,
                    seen,
                    &child_run_as,
                )?;
                result.extend(child_items);
            }
        }
    }
    Ok(result)
}

fn parse_step(node: &kdl::KdlNode) -> Result<Step, GlideshError> {
    let name = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "Step requires a name argument".to_string(),
        })?
        .to_string();

    let loop_source = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("loop"))
        .and_then(|e| e.value().as_string())
        .map(|s| {
            if s.starts_with("${") && s.ends_with('}') {
                LoopSource::Variable(s[2..s.len() - 1].to_string())
            } else {
                LoopSource::Literal(
                    s.lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect(),
                )
            }
        });

    let subscribe: Vec<String> = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("subscribe"))
        .and_then(|e| e.value().as_string())
        .map(|s| {
            s.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default();

    reject_unknown_step_attrs(node, &name)?;

    let when = parse_when(node)?;
    if let Some(cond) = &when {
        if let Some(var) = cond.variables().find(|v| is_item_var(v)) {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "step '{name}': when= cannot use ${{{var}}}: a step's condition is \
                     checked once, before its loop runs. Put when= on the task instead to \
                     test each item"
                ),
            });
        }
    }

    let mut tasks = Vec::new();

    if let Some(children) = node.children() {
        for task_node in children.nodes() {
            tasks.push(parse_task(task_node)?);
        }
    }

    let run_as = super::parse_run_as_attrs(node)?;

    Ok(Step {
        name,
        tasks,
        loop_source,
        subscribe,
        run_as,
        when,
    })
}

const STEP_ATTRS: &[&str] = &["loop", "subscribe", "when", "run-as", "run-as-method"];

/// A misspelled step attribute used to be ignored, which for `when=` means a guard that
/// silently never applies — the step runs unconditionally.
fn reject_unknown_step_attrs(node: &kdl::KdlNode, step: &str) -> Result<(), GlideshError> {
    for entry in node.entries() {
        if let Some(key) = entry.name().map(|n| n.value()) {
            if !STEP_ATTRS.contains(&key) {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "step '{step}': unknown attribute '{key}' (expected one of: {})",
                        STEP_ATTRS.join(", ")
                    ),
                });
            }
        }
    }
    Ok(())
}

fn parse_when(node: &kdl::KdlNode) -> Result<Option<Condition>, GlideshError> {
    let Some(entry) = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some("when"))
    else {
        return Ok(None);
    };
    let source = entry
        .value()
        .as_string()
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "when= must be a string, e.g. when=\"${@os.family} == debian\"".into(),
        })?;
    Condition::parse(source).map(Some)
}

fn is_item_var(name: &str) -> bool {
    name == "@item" || name.starts_with("@item.")
}

fn parse_task(node: &kdl::KdlNode) -> Result<TaskDef, GlideshError> {
    let node_name = node.name().to_string();

    let positional: Vec<&str> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .filter_map(|e| e.value().as_string())
        .collect();

    let (module, resource) = if node_name == "external" {
        let mod_name = positional
            .first()
            .ok_or_else(|| GlideshError::ConfigParse {
                message: "external requires a module name argument".into(),
            })?;
        let res = positional.get(1).unwrap_or(&"");
        (format!("external.{}", mod_name), res.to_string())
    } else {
        let res = positional.first().unwrap_or(&"");
        (node_name, res.to_string())
    };

    let mut args = HashMap::new();
    let mut register = None;

    for entry in node.entries() {
        if let Some(name) = entry.name() {
            let key = name.to_string();
            if key == "register" {
                register = entry.value().as_string().map(|s| s.to_string());
                if let Some(ref name) = register {
                    super::validate_user_var_name(name)?;
                }
            } else if key == "run-as" || key == "run-as-method" || key == "when" {
                // Captured separately, not a module arg.
            } else {
                let value = kdl_value_to_param(entry.value());
                args.insert(key, value);
            }
        }
    }

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().to_string();

            // A child node would otherwise become a module argument, leaving the task
            // unguarded — so it must fail rather than run.
            if key == "when" {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "{module} '{resource}': write the condition as an attribute, \
                         when=\"...\", not as a child node"
                    ),
                });
            }

            if child.children().is_some()
                && child
                    .children()
                    .unwrap()
                    .nodes()
                    .iter()
                    .all(|n| n.name().to_string() == "-")
            {
                let list: Vec<String> = child
                    .children()
                    .unwrap()
                    .nodes()
                    .iter()
                    .filter_map(|n| {
                        n.entries()
                            .iter()
                            .find(|e| e.name().is_none())
                            .and_then(|e| e.value().as_string())
                            .map(|s| s.to_string())
                    })
                    .collect();
                args.insert(key, ParamValue::List(list));
            } else if child.children().is_some() {
                let mut map = HashMap::new();
                for mapnode in child.children().unwrap().nodes() {
                    let mk = mapnode.name().to_string();
                    let mv = mapnode
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .map(|e| super::kdl_value_to_string(e.value()))
                        .unwrap_or_default();
                    map.insert(mk, mv);
                }
                args.insert(key, ParamValue::Map(map));
            } else {
                let value = child
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .map(|e| kdl_value_to_param(e.value()))
                    .unwrap_or(ParamValue::String(String::new()));
                args.insert(key, value);
            }
        }
    }

    // Other modules would ignore it, or reject it only when the task runs.
    if let Some(value) = args.get("changed-when") {
        if module != "shell" {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "{module} '{resource}': changed-when is only supported on shell tasks"
                ),
            });
        }
        let valid = match value {
            ParamValue::Bool(_) => true,
            ParamValue::String(cmd) => !cmd.trim().is_empty(),
            _ => false,
        };
        if !valid {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "shell '{resource}': changed-when must be #false, #true, or a command"
                ),
            });
        }
    }

    let run_as = super::parse_run_as_attrs(node)?;
    let when = parse_when(node)?;

    Ok(TaskDef {
        module,
        resource,
        args,
        register,
        run_as,
        when,
    })
}

fn kdl_value_to_param(value: &kdl::KdlValue) -> ParamValue {
    match value {
        kdl::KdlValue::String(s) => ParamValue::String(s.clone()),
        kdl::KdlValue::Integer(i) => ParamValue::Integer(*i as i64),
        kdl::KdlValue::Bool(b) => ParamValue::Bool(*b),
        kdl::KdlValue::Float(f) => ParamValue::String(f.to_string()),
        kdl::KdlValue::Null => ParamValue::String(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_plan() {
        let input = r#"
plan "deploy-app" {
    mode "sync"

    vars {
        app-image "registry.example.com/myapp:latest"
        app-port 8080
    }

    step "Install base packages" {
        package "nginx" state="present"
        package "curl" state="present"
    }

    step "Health check" {
        shell "curl -sf http://localhost:8080/health" {
            retries 5
            delay 3
        }
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.name, "deploy-app");
        assert_eq!(fp.mode, ExecutionMode::Sync);
        assert_eq!(fp.vars.get("app-port").unwrap(), "8080");
        assert_eq!(fp.steps().len(), 2);
        assert_eq!(fp.steps()[0].name, "Install base packages");
        assert_eq!(fp.steps()[0].tasks.len(), 2);
        assert_eq!(fp.steps()[0].tasks[0].module, "package");
        assert_eq!(fp.steps()[0].tasks[0].resource, "nginx");
        assert_eq!(
            fp.steps()[0].tasks[0].args.get("state").unwrap().as_str(),
            Some("present")
        );
        assert_eq!(fp.steps()[1].tasks[0].module, "shell");
        assert_eq!(
            fp.steps()[1].tasks[0].args.get("retries").unwrap().as_i64(),
            Some(5)
        );
    }

    #[test]
    fn test_parse_plan_level_run_as() {
        use crate::config::types::{RunAsMethod, RunAsUser};
        let input = r#"plan "p" run-as="root" run-as-method="doas" { step "s" { shell "id" } }"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.run_as.user, Some(RunAsUser::User("root".to_string())));
        assert_eq!(fp.run_as.method, Some(RunAsMethod::Doas));
    }

    #[test]
    fn test_parse_run_as_step_and_task() {
        use crate::config::types::{RunAsMethod, RunAsUser};
        let input = r#"
plan "p" {
    step "Install" run-as="root" {
        package "nginx" state="present"
        shell "whoami" run-as="postgres" run-as-method="doas"
        shell "id" run-as=""
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let step = &fp.steps()[0];
        assert_eq!(step.run_as.user, Some(RunAsUser::User("root".to_string())));

        // Module without a run-as attribute inherits (None).
        assert_eq!(step.tasks[0].run_as.user, None);

        // Module-level override, and run-as attrs must not leak into module args.
        assert_eq!(
            step.tasks[1].run_as.user,
            Some(RunAsUser::User("postgres".to_string()))
        );
        assert_eq!(step.tasks[1].run_as.method, Some(RunAsMethod::Doas));
        assert!(!step.tasks[1].args.contains_key("run-as"));
        assert!(!step.tasks[1].args.contains_key("run-as-method"));

        // run-as="" opts out at the task level.
        assert_eq!(step.tasks[2].run_as.user, Some(RunAsUser::Disabled));
    }

    #[test]
    fn test_parse_container_task() {
        let input = r#"
plan "containers" {
    step "Deploy app" {
        container "myapp" {
            image "registry.example.com/myapp:latest"
            state "running"
            ports {
                - "8080:80"
            }
            environment {
                DATABASE_URL "postgres://db:5432/app"
            }
        }
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.module, "container");
        assert_eq!(task.resource, "myapp");
        assert_eq!(
            task.args.get("image").unwrap().as_str(),
            Some("registry.example.com/myapp:latest")
        );
        let ports = task.args.get("ports").unwrap().as_list().unwrap();
        assert_eq!(ports, &["8080:80"]);
        let env = task.args.get("environment").unwrap().as_map().unwrap();
        assert_eq!(env.get("DATABASE_URL").unwrap(), "postgres://db:5432/app");
    }

    #[test]
    fn test_parse_container_with_command() {
        let input = r#"
plan "containers" {
    step "Deploy app" {
        container "myapp" {
            image "python:3.12-slim"
            command "python -m http.server 8000"
            ports {
                - "8000:8000"
            }
        }
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.module, "container");
        assert_eq!(task.resource, "myapp");
        assert_eq!(
            task.args.get("image").unwrap().as_str(),
            Some("python:3.12-slim")
        );
        assert_eq!(
            task.args.get("command").unwrap().as_str(),
            Some("python -m http.server 8000")
        );
        let ports = task.args.get("ports").unwrap().as_list().unwrap();
        assert_eq!(ports, &["8000:8000"]);
    }

    #[test]
    fn test_parse_register() {
        let input = r#"
plan "test" {
    step "Get disks" {
        shell "lsblk" register="available_disks"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.register, Some("available_disks".to_string()));
        assert!(!task.args.contains_key("register"));
    }

    #[test]
    fn test_parse_loop_variable() {
        let input = r#"
plan "test" {
    step "Format each" loop="${disks}" {
        disk "${@item}" fs="ext4"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(
            fp.steps()[0].loop_source,
            Some(LoopSource::Variable("disks".to_string()))
        );
    }

    #[test]
    fn test_parse_register_with_raw_string() {
        let input = "plan \"test\" {\n    step \"List disks\" {\n        shell #\"lsblk -dn -o NAME | sed 's/^/\\/dev\\///'\"# register=\"available_disks\"\n    }\n}\n";
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.module, "shell");
        assert!(task.resource.contains("lsblk"));
        assert_eq!(task.register, Some("available_disks".to_string()));
        assert!(!task.args.contains_key("register"));
    }

    #[test]
    fn test_parse_no_loop_no_register() {
        let input = r#"
plan "test" {
    step "Simple" {
        shell "echo hello"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert!(fp.steps()[0].loop_source.is_none());
        assert!(fp.steps()[0].tasks[0].register.is_none());
    }

    #[test]
    fn test_parse_include() {
        let input = r#"
plan "main" {
    step "First" {
        shell "echo first"
    }
    include "common/security.kdl"
    step "Last" {
        shell "echo last"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.items.len(), 3);
        assert_eq!(fp.steps().len(), 2);
        matches!(&fp.items[1], PlanItem::Include(p) if p == "common/security.kdl");
    }

    /// Like `mode`, these belong to whichever plan is run: a plan written to work both on its
    /// own and included elsewhere must not impose its rollout on the plan including it.
    #[test]
    fn an_included_plans_rollout_settings_do_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("child.kdl"),
            "plan \"child\" {\n    serial 5\n    max-fail 0\n    step \"c\" { shell \"true\" }\n}",
        )
        .unwrap();
        let mut plan =
            parse_plan("plan \"parent\" {\n    serial 2\n    include \"child.kdl\"\n}").unwrap();
        resolve_includes(&mut plan, dir.path()).unwrap();
        assert_eq!(plan.serial, [Amount::Count(2)]);
        assert_eq!(plan.max_fail, None);
        assert_eq!(plan.steps().len(), 1);
    }

    #[test]
    fn test_resolve_includes_flattens() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("glidesh_test_includes");
        let _ = std::fs::create_dir_all(&dir);

        let child = r#"
plan "child" {
    step "Child step" {
        shell "echo child"
    }
}
"#;
        let child_path = dir.join("child.kdl");
        let mut f = std::fs::File::create(&child_path).unwrap();
        f.write_all(child.as_bytes()).unwrap();

        let parent = r#"
plan "parent" {
    step "Before" {
        shell "echo before"
    }
    include "child.kdl"
    step "After" {
        shell "echo after"
    }
}
"#;
        let mut plan = parse_plan(parent).unwrap();
        resolve_includes(&mut plan, &dir).unwrap();

        assert_eq!(plan.steps().len(), 3);
        assert_eq!(plan.steps()[0].name, "Before");
        assert_eq!(plan.steps()[1].name, "Child step");
        assert_eq!(plan.steps()[2].name, "After");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_included_plan_run_as_propagates_to_its_steps() {
        use crate::config::types::{RunAsMethod, RunAsUser};
        use std::io::Write;
        let dir = std::env::temp_dir().join("glidesh_test_include_runas");
        let _ = std::fs::create_dir_all(&dir);

        // The included plan declares plan-level escalation; its own steps must
        // inherit it after flattening, while a step that opts out still wins.
        let child = r#"
plan "child" run-as="root" run-as-method="doas" {
    step "Inherits" {
        shell "id"
    }
    step "Opts out" run-as="" {
        shell "whoami"
    }
}
"#;
        let mut f = std::fs::File::create(dir.join("child.kdl")).unwrap();
        f.write_all(child.as_bytes()).unwrap();

        // The parent has no plan-level run-as of its own.
        let parent = r#"
plan "parent" {
    step "Local" {
        shell "echo hi"
    }
    include "child.kdl"
}
"#;
        let mut plan = parse_plan(parent).unwrap();
        resolve_includes(&mut plan, &dir).unwrap();

        let steps = plan.steps();
        assert_eq!(steps[0].name, "Local");
        assert_eq!(steps[0].run_as.user, None);

        assert_eq!(steps[1].name, "Inherits");
        assert_eq!(
            steps[1].run_as.user,
            Some(RunAsUser::User("root".to_string()))
        );
        assert_eq!(steps[1].run_as.method, Some(RunAsMethod::Doas));

        assert_eq!(steps[2].name, "Opts out");
        assert_eq!(steps[2].run_as.user, Some(RunAsUser::Disabled));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_loop_literal() {
        let input = r#"
plan "test" {
    step "Iterate" loop="alpha" {
        shell "echo ${@item}"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(
            fp.steps()[0].loop_source,
            Some(LoopSource::Literal(vec!["alpha".to_string()]))
        );
    }

    #[test]
    fn test_parse_register_and_loop_combined() {
        let input = r#"
plan "test" {
    step "Discover" {
        shell "ls /dev" register="devices"
    }
    step "Process" loop="${devices}" {
        shell "echo ${@item}"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.steps().len(), 2);
        assert_eq!(fp.steps()[0].tasks[0].register, Some("devices".to_string()));
        assert_eq!(
            fp.steps()[1].loop_source,
            Some(LoopSource::Variable("devices".to_string()))
        );
    }

    #[test]
    fn test_resolve_includes_circular() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("glidesh_test_circular");
        let _ = std::fs::create_dir_all(&dir);

        let a = r#"
plan "plan-a" {
    include "b.kdl"
}
"#;
        let b = r#"
plan "plan-b" {
    include "a.kdl"
}
"#;
        std::fs::File::create(dir.join("a.kdl"))
            .unwrap()
            .write_all(a.as_bytes())
            .unwrap();
        std::fs::File::create(dir.join("b.kdl"))
            .unwrap()
            .write_all(b.as_bytes())
            .unwrap();

        let mut plan = parse_plan(a).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Circular include"),
            "expected circular include error, got: {}",
            msg
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_includes_nested() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("glidesh_test_nested_includes");
        let _ = std::fs::create_dir_all(dir.join("sub"));

        let grandchild = r#"
plan "grandchild" {
    step "GC step" {
        shell "echo grandchild"
    }
}
"#;
        std::fs::File::create(dir.join("sub/grandchild.kdl"))
            .unwrap()
            .write_all(grandchild.as_bytes())
            .unwrap();

        let child = r#"
plan "child" {
    step "Child step" {
        shell "echo child"
    }
    include "sub/grandchild.kdl"
}
"#;
        std::fs::File::create(dir.join("child.kdl"))
            .unwrap()
            .write_all(child.as_bytes())
            .unwrap();

        let parent = r#"
plan "parent" {
    step "Parent step" {
        shell "echo parent"
    }
    include "child.kdl"
}
"#;
        let mut plan = parse_plan(parent).unwrap();
        resolve_includes(&mut plan, &dir).unwrap();

        assert_eq!(plan.steps().len(), 3);
        assert_eq!(plan.steps()[0].name, "Parent step");
        assert_eq!(plan.steps()[1].name, "Child step");
        assert_eq!(plan.steps()[2].name, "GC step");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_includes_vars_merge() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("glidesh_test_include_vars");
        let _ = std::fs::create_dir_all(&dir);

        let child = r#"
plan "child" {
    vars {
        from-child "child-value"
        shared "child-version"
    }
    step "Child" {
        shell "echo"
    }
}
"#;
        std::fs::File::create(dir.join("child.kdl"))
            .unwrap()
            .write_all(child.as_bytes())
            .unwrap();

        let parent = r#"
plan "parent" {
    vars {
        shared "parent-version"
    }
    include "child.kdl"
}
"#;
        let mut plan = parse_plan(parent).unwrap();
        // Parent var "shared" should win over child's
        assert_eq!(plan.vars.get("shared").unwrap(), "parent-version");

        resolve_includes(&mut plan, &dir).unwrap();
        // After resolution, plan.vars is still the parent's vars
        assert_eq!(plan.vars.get("shared").unwrap(), "parent-version");
        // The child's unique var isn't merged into parent.vars
        // (vars merge happens at runtime in node_runner, not in resolve_includes)
        assert!(!plan.vars.contains_key("from-child"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_external_module() {
        let input = r#"
plan "test" {
    step "Configure nginx" {
        external "acme/nginx-vhost" "mysite" server_name="example.com"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.module, "external.acme/nginx-vhost");
        assert_eq!(task.resource, "mysite");
        assert_eq!(
            task.args.get("server_name").unwrap().as_str(),
            Some("example.com")
        );
    }

    #[test]
    fn test_parse_external_module_no_resource() {
        let input = r#"
plan "test" {
    step "Run plugin" {
        external "acme/cleanup" timeout=30
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        let task = &fp.steps()[0].tasks[0];
        assert_eq!(task.module, "external.acme/cleanup");
        assert_eq!(task.resource, "");
    }

    #[test]
    fn test_parse_external_module_missing_name() {
        let input = r#"
plan "test" {
    step "Bad" {
        external server_name="example.com"
    }
}
"#;
        let result = parse_plan(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("external requires a module name"));
    }

    #[test]
    fn test_parse_structured_vars() {
        let input = r#"
plan "test" {
    vars {
        api-keys {
            - name="k1" value="sk-aaa"
            - name="k2" value="sk-bbb"
        }
    }
    step "Deploy" {
        shell "echo"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert!(!fp.vars.contains_key("api-keys"));
        let keys = fp.structured_vars.get("api-keys").unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].get("name").unwrap(), "k1");
        assert_eq!(keys[0].get("value").unwrap(), "sk-aaa");
        assert_eq!(keys[1].get("name").unwrap(), "k2");
        assert_eq!(keys[1].get("value").unwrap(), "sk-bbb");
    }

    #[test]
    fn test_parse_mixed_vars() {
        let input = r#"
plan "test" {
    vars {
        simple-var "hello"
        port 8080
        items {
            - key="a" val="1"
        }
    }
    step "Do" {
        shell "echo"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.vars.get("simple-var").unwrap(), "hello");
        assert_eq!(fp.vars.get("port").unwrap(), "8080");
        assert!(!fp.vars.contains_key("items"));
        let items = fp.structured_vars.get("items").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get("key").unwrap(), "a");
    }

    #[test]
    fn test_structured_vars_not_list() {
        // Plain lists (- "item") should NOT be treated as structured vars
        let input = r#"
plan "test" {
    vars {
        tags "dev"
    }
    step "Do" {
        shell "echo"
    }
}
"#;
        let fp = parse_plan(input).unwrap();
        assert_eq!(fp.vars.get("tags").unwrap(), "dev");
        assert!(fp.structured_vars.is_empty());
    }

    #[test]
    fn test_vars_file_basic() {
        use std::io::Write;
        let dir =
            std::env::temp_dir().join(format!("glidesh_test_vars_file_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let vars_content = r#"
region "us-east-1"
api-keys {
    - name="k1" value="sk-aaa"
    - name="k2" value="sk-bbb"
}
"#;
        let vars_path = dir.join("keys.kdl");
        std::fs::File::create(&vars_path)
            .unwrap()
            .write_all(vars_content.as_bytes())
            .unwrap();

        let plan_input = r#"
plan "test" {
    vars-file "keys.kdl"
    step "Do" {
        shell "echo"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        assert_eq!(plan.vars_files.len(), 1);

        resolve_includes(&mut plan, &dir).unwrap();

        assert_eq!(plan.vars.get("region").unwrap(), "us-east-1");
        let keys = plan.structured_vars.get("api-keys").unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].get("name").unwrap(), "k1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vars_file_inline_wins() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "glidesh_test_vars_file_override_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);

        let vars_content = r#"
region "from-file"
"#;
        std::fs::File::create(dir.join("ext.kdl"))
            .unwrap()
            .write_all(vars_content.as_bytes())
            .unwrap();

        let plan_input = r#"
plan "test" {
    vars {
        region "inline-wins"
    }
    vars-file "ext.kdl"
    step "Do" {
        shell "echo"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        resolve_includes(&mut plan, &dir).unwrap();

        // Inline var should win over vars-file
        assert_eq!(plan.vars.get("region").unwrap(), "inline-wins");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vars_file_missing() {
        let dir = std::env::temp_dir().join(format!(
            "glidesh_test_vars_file_missing_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);

        let plan_input = r#"
plan "test" {
    vars-file "nonexistent.kdl"
    step "Do" {
        shell "echo"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent.kdl"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_duplicate_var_in_plan_vars_block() {
        let input = r#"
plan "test" {
    vars {
        region "us-east-1"
        region "eu-west-1"
    }
    step "Do" {
        shell "echo"
    }
}
"#;
        let result = parse_plan(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'region'"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_structured_var_in_plan_vars_block() {
        let input = r#"
plan "test" {
    vars {
        keys {
            - name="k1" value="v1"
        }
        keys {
            - name="k2" value="v2"
        }
    }
    step "Do" {
        shell "echo"
    }
}
"#;
        let result = parse_plan(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'keys'"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_var_in_vars_file() {
        use std::io::Write;
        let dir =
            std::env::temp_dir().join(format!("glidesh_test_dup_vars_file_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let vars_content = r#"
region "us-east-1"
region "eu-west-1"
"#;
        std::fs::File::create(dir.join("dup.kdl"))
            .unwrap()
            .write_all(vars_content.as_bytes())
            .unwrap();

        let plan_input = r#"
plan "test" {
    vars-file "dup.kdl"
    step "Do" {
        shell "echo"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'region'"), "got: {}", msg);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_duplicate_var_across_vars_files() {
        use std::io::Write;
        let dir =
            std::env::temp_dir().join(format!("glidesh_test_dup_across_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        std::fs::File::create(dir.join("a.kdl"))
            .unwrap()
            .write_all(b"region \"us-east-1\"")
            .unwrap();
        std::fs::File::create(dir.join("b.kdl"))
            .unwrap()
            .write_all(b"region \"eu-west-1\"")
            .unwrap();

        let plan_input = r#"
plan "test" {
    vars-file "a.kdl"
    vars-file "b.kdl"
    step "Do" {
        shell "echo"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("region") && msg.contains("a.kdl") && msg.contains("b.kdl"),
            "got: {}",
            msg
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserved_at_var_name_rejected_in_plan() {
        let input = "plan \"p\" {\n    vars {\n        \"@item\" \"x\"\n    }\n    step \"s\" { shell \"echo\" }\n}";
        let err = parse_plan(input).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
    }

    /// A quoted KDL name can start with `@`, so this is the route a plan would take to
    /// spoof a detected fact.
    #[test]
    fn a_plan_cannot_spoof_an_os_fact() {
        let input = "plan \"p\" {\n    vars {\n        \"@os.family\" \"debian\"\n    }\n    step \"s\" { shell \"echo\" }\n}";
        let err = parse_plan(input).unwrap_err().to_string();
        assert!(
            err.contains("reserved") && err.contains("@os"),
            "got: {err}"
        );
    }

    #[test]
    fn a_plain_os_var_does_not_collide_with_the_os_namespace() {
        let input = "plan \"p\" {\n    vars {\n        os \"custom\"\n    }\n    step \"s\" { shell \"echo\" }\n}";
        let plan = parse_plan(input).unwrap();
        assert_eq!(plan.vars.get("os").map(String::as_str), Some("custom"));
    }

    fn one_step(body: &str) -> Step {
        let plan = parse_plan(&format!("plan \"p\" {{\n{body}\n}}")).unwrap();
        plan.steps().into_iter().next().unwrap().clone()
    }

    fn plan_err(body: &str) -> String {
        parse_plan(&format!("plan \"p\" {{\n{body}\n}}"))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn when_is_parsed_on_steps_and_tasks() {
        let step = one_step(
            r#"step "s" when="${@os.family} == debian" {
                shell "echo" when="defined ${x}"
            }"#,
        );
        assert_eq!(
            step.when.as_ref().map(|c| c.source()),
            Some("${@os.family} == debian")
        );
        assert_eq!(
            step.tasks[0].when.as_ref().map(|c| c.source()),
            Some("defined ${x}")
        );
    }

    #[test]
    fn when_is_not_passed_to_the_module() {
        let step = one_step(r#"step "s" { shell "echo" when="${x}" }"#);
        assert!(!step.tasks[0].args.contains_key("when"));
    }

    #[test]
    fn a_step_without_when_has_no_condition() {
        let step = one_step(r#"step "s" { shell "echo" }"#);
        assert!(step.when.is_none() && step.tasks[0].when.is_none());
    }

    #[test]
    fn a_malformed_when_fails_to_parse() {
        let err = plan_err(r#"step "s" when="${a} = b" { shell "echo" }"#);
        assert!(err.contains("invalid when="), "{err}");
        let err = plan_err(r#"step "s" { shell "echo" when="${a} >" }"#);
        assert!(err.contains("invalid when="), "{err}");
    }

    #[test]
    fn when_must_be_a_string() {
        let err = plan_err(r#"step "s" when=#true { shell "echo" }"#);
        assert!(err.contains("must be a string"), "{err}");
    }

    /// A step's condition runs before its loop, when `@item` does not exist yet.
    #[test]
    fn a_step_condition_cannot_read_the_loop_item() {
        for cond in ["${@item} == a", "${@item.name} == a"] {
            let err = plan_err(&format!(
                r#"step "s" loop="${{xs}}" when="{cond}" {{ shell "echo" }}"#
            ));
            assert!(err.contains("Put when= on the task"), "{err}");
        }
    }

    #[test]
    fn a_task_condition_may_read_the_loop_item() {
        let step = one_step(r#"step "s" loop="${xs}" { shell "echo" when="${@item} != a" }"#);
        assert!(step.tasks[0].when.is_some());
    }

    /// A misspelled `when` must not leave a step running unguarded.
    #[test]
    fn an_unknown_step_attribute_is_rejected() {
        let err = plan_err(r#"step "s" wehn="${x}" { shell "echo" }"#);
        assert!(err.contains("unknown attribute 'wehn'"), "{err}");
    }

    #[test]
    fn every_documented_step_attribute_is_accepted() {
        one_step(
            r#"step "a" { shell "echo" }
            step "s" loop="${xs}" subscribe="a" when="${x}" run-as="root" run-as-method="sudo" {
                shell "echo"
            }"#,
        );
    }

    fn rollout(body: &str) -> Result<Plan, String> {
        parse_plan(&format!("plan \"p\" {{\n{body}\n}}")).map_err(|e| e.to_string())
    }

    #[test]
    fn serial_takes_counts_and_percentages_in_order() {
        let p = rollout(r#"serial 1 "25%" "3""#).unwrap();
        assert_eq!(
            p.serial,
            [Amount::Count(1), Amount::Percent(25), Amount::Count(3)]
        );
    }

    #[test]
    fn without_serial_or_max_fail_nothing_is_set() {
        let p = rollout(r#"step "s" { shell "true" }"#).unwrap();
        assert!(p.serial.is_empty() && p.max_fail.is_none());
    }

    #[test]
    fn max_fail_takes_one_count_or_percentage() {
        assert_eq!(
            rollout("max-fail 0").unwrap().max_fail,
            Some(Amount::Count(0))
        );
        assert_eq!(
            rollout(r#"max-fail "10%""#).unwrap().max_fail,
            Some(Amount::Percent(10))
        );
        assert_eq!(
            rollout(r#"max-fail "0%""#).unwrap().max_fail,
            Some(Amount::Percent(0))
        );
    }

    #[test]
    fn bad_rollout_values_are_rejected() {
        for (body, expect) in [
            ("serial", "at least one batch size"),
            ("serial 0", "at least 1"),
            ("serial -2", "at least 1"),
            (r#"serial "0%""#, "at least 1"),
            (r#"serial "150%""#, "100%"),
            (r#"serial "half""#, "\"half\""),
            ("max-fail -1", "at least 0"),
            (r#"max-fail "101%""#, "100%"),
            ("max-fail", "one value"),
            ("max-fail 1 2", "one value"),
        ] {
            let err = rollout(body).unwrap_err();
            assert!(err.contains(expect), "{body}: {err}");
        }
    }

    #[test]
    fn a_mode_must_be_sync_or_async() {
        for (body, expect) in [
            (r#"mode "sync""#, Some(ExecutionMode::Sync)),
            (r#"mode "async""#, Some(ExecutionMode::Async)),
            (r#"mode "asinc""#, None),
            ("mode", None),
        ] {
            let parsed = parse_plan(&format!("plan \"p\" {{\n{body}\n}}"));
            match expect {
                Some(mode) => assert_eq!(parsed.unwrap().mode, mode, "{body}"),
                None => {
                    let err = parsed.unwrap_err().to_string();
                    assert!(err.contains("\"sync\" or \"async\""), "{body}: {err}");
                }
            }
        }
    }

    #[test]
    fn changed_when_is_accepted_on_shell() {
        let step = one_step(
            r#"step "s" {
                shell "lsblk" changed-when=#false
                shell "apt-get upgrade -y" changed-when="test -f /var/run/reboot-required"
            }"#,
        );
        assert_eq!(
            step.tasks[0]
                .args
                .get("changed-when")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn changed_when_is_rejected_off_shell() {
        let err = plan_err(r#"step "s" { package "nginx" changed-when=#false }"#);
        assert!(err.contains("only supported on shell"), "{err}");
    }

    #[test]
    fn changed_when_must_be_a_bool_or_a_command() {
        for bad in ["1", r#""""#, r#""   ""#] {
            let err = plan_err(&format!(
                r#"step "s" {{ shell "true" changed-when={bad} }}"#
            ));
            assert!(
                err.contains("must be #false, #true, or a command"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn when_as_a_child_node_is_rejected() {
        let err = plan_err(
            r#"step "s" {
                file "/etc/x" {
                    src "x"
                    when "${x}"
                }
            }"#,
        );
        assert!(err.contains("as an attribute"), "{err}");
    }

    #[test]
    fn reserved_at_register_name_rejected() {
        let input = "plan \"p\" {\n    step \"s\" { shell \"echo\" register=\"@out\" }\n}";
        let err = parse_plan(input).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn test_parse_subscribe() {
        let input = r#"
plan "test" {
    step "Deploy config" {
        shell "echo deploy"
    }
    step "Restart app" subscribe="Deploy config" {
        shell "echo restart"
    }
}
"#;
        let plan = parse_plan(input).unwrap();
        let steps = plan.steps();
        assert!(steps[0].subscribe.is_empty());
        assert_eq!(steps[1].subscribe, vec!["Deploy config"]);
    }

    #[test]
    fn test_parse_subscribe_comma_separated() {
        let input = r#"
plan "test" {
    step "Upload files" {
        shell "echo upload"
    }
    step "Deploy config" {
        shell "echo deploy"
    }
    step "Restart app" subscribe="Upload files, Deploy config" {
        shell "echo restart"
    }
}
"#;
        let plan = parse_plan(input).unwrap();
        let steps = plan.steps();
        assert_eq!(steps[2].subscribe, vec!["Upload files", "Deploy config"]);
    }

    #[test]
    fn test_subscribe_rejects_forward_reference() {
        let input = r#"
plan "test" {
    step "Restart app" subscribe="Deploy config" {
        shell "echo restart"
    }
    step "Deploy config" {
        shell "echo deploy"
    }
}
"#;
        let mut plan = parse_plan(input).unwrap();
        let dir = std::env::temp_dir().join(format!("glidesh_sub_fwd_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Deploy config"), "got: {}", msg);
        assert!(msg.contains("not a preceding step"), "got: {}", msg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subscribe_rejects_unknown_step() {
        let input = r#"
plan "test" {
    step "Restart app" subscribe="Nonexistent" {
        shell "echo restart"
    }
}
"#;
        let mut plan = parse_plan(input).unwrap();
        let dir = std::env::temp_dir().join(format!("glidesh_sub_unk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Nonexistent"), "got: {}", msg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subscribe_rejects_duplicate_step_names() {
        let input = r#"
plan "test" {
    step "Deploy" {
        shell "echo first"
    }
    step "Deploy" {
        shell "echo second"
    }
}
"#;
        let mut plan = parse_plan(input).unwrap();
        let dir = std::env::temp_dir().join(format!("glidesh_sub_dup_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate step name"), "got: {}", msg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subscribe_to_included_step() {
        let dir = std::env::temp_dir().join(format!("glidesh_sub_inc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("setup.kdl"),
            r#"plan "setup" {
    step "Deploy config" {
        shell "echo deploy"
    }
}"#,
        )
        .unwrap();

        let plan_input = r#"
plan "main" {
    include "setup.kdl"
    step "Restart app" subscribe="Deploy config" {
        shell "echo restart"
    }
}
"#;
        let mut plan = parse_plan(plan_input).unwrap();
        let result = resolve_includes(&mut plan, &dir);
        assert!(result.is_ok(), "got: {}", result.unwrap_err());
        let steps = plan.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].subscribe, vec!["Deploy config"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shell_cmd_string() {
        let input = r#"
plan "test" {
    step "Start container" {
        shell {
            check "docker ps --filter name=myapp --filter status=running -q | grep -q ."
            cmd "docker run -d --name myapp nginx:latest"
        }
    }
}
"#;
        let plan = parse_plan(input).unwrap();
        let task = &plan.steps()[0].tasks[0];
        assert_eq!(task.module, "shell");
        assert_eq!(task.resource, "");
        assert_eq!(
            task.args.get("cmd").unwrap().as_str(),
            Some("docker run -d --name myapp nginx:latest")
        );
        assert_eq!(
            task.args.get("check").unwrap().as_str(),
            Some("docker ps --filter name=myapp --filter status=running -q | grep -q .")
        );
    }
}
