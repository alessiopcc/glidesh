use crate::config::types::{ExecutionMode, LoopSource, ParamValue, Plan, PlanItem, Step, TaskDef};
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
                    .and_then(|e| e.value().as_string())
                    .unwrap_or("sync");
                mode = match mode_str {
                    "async" => ExecutionMode::Async,
                    _ => ExecutionMode::Sync,
                };
            }
            "vars" => {
                if let Some(vc) = node.children() {
                    for vnode in vc.nodes() {
                        let key = vnode.name().to_string();
                        if let Some(list_of_maps) = parse_structured_var(vnode) {
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
                                .map(|e| kdl_value_to_string(e.value()))
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

    Ok(Plan {
        name,
        mode,
        vars,
        structured_vars,
        vars_files,
        items,
    })
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
    let resolved = resolve_items(
        &plan.items,
        &plan.vars,
        &plan.structured_vars,
        base_dir,
        &mut seen,
    )?;
    plan.items = resolved;
    Ok(())
}

/// Load vars from external KDL files. Each file contains raw var nodes (no wrapper).
/// Inline vars take precedence over vars-file vars (loaded first, inline overwrites).
fn resolve_vars_files(
    paths: &[String],
    vars: &mut HashMap<String, String>,
    structured_vars: &mut HashMap<String, Vec<HashMap<String, String>>>,
    base_dir: &Path,
) -> Result<(), GlideshError> {
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
            if let Some(list_of_maps) = parse_structured_var(vnode) {
                // Inline structured vars win — only insert if not already present
                structured_vars.entry(key).or_insert(list_of_maps);
            } else {
                let value = vnode
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .map(|e| kdl_value_to_string(e.value()))
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
) -> Result<Vec<PlanItem>, GlideshError> {
    let mut result = Vec::new();
    for item in items {
        match item {
            PlanItem::Step(s) => result.push(PlanItem::Step(s.clone())),
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

                let child_base = resolved_path.parent().unwrap_or(base_dir);
                let child_items = resolve_items(
                    &included.items,
                    &merged_vars,
                    &merged_structured,
                    child_base,
                    seen,
                )?;
                result.extend(child_items);
            }
        }
    }
    Ok(result)
}

/// Detect whether a vars node is a structured list-of-maps (for template loops).
///
/// Returns `Some(list)` if the node has children that are all `"-"` nodes
/// with at least one named property. Returns `None` for scalar or plain-list vars.
fn parse_structured_var(node: &kdl::KdlNode) -> Option<Vec<HashMap<String, String>>> {
    let children = node.children()?;
    let nodes = children.nodes();
    if nodes.is_empty() {
        return None;
    }
    if !nodes.iter().all(|n| n.name().to_string() == "-") {
        return None;
    }
    // Must have at least one named property to distinguish from plain lists
    let has_named = nodes
        .iter()
        .any(|n| n.entries().iter().any(|e| e.name().is_some()));
    if !has_named {
        return None;
    }

    let items = nodes
        .iter()
        .map(|n| {
            let mut map = HashMap::new();
            for entry in n.entries() {
                if let Some(name) = entry.name() {
                    map.insert(name.to_string(), kdl_value_to_string(entry.value()));
                }
            }
            map
        })
        .collect();
    Some(items)
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

    let mut tasks = Vec::new();

    if let Some(children) = node.children() {
        for task_node in children.nodes() {
            tasks.push(parse_task(task_node)?);
        }
    }

    Ok(Step {
        name,
        tasks,
        loop_source,
    })
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
            } else {
                let value = kdl_value_to_param(entry.value());
                args.insert(key, value);
            }
        }
    }

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().to_string();

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
                        .map(|e| kdl_value_to_string(e.value()))
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

    Ok(TaskDef {
        module,
        resource,
        args,
        register,
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

fn kdl_value_to_string(value: &kdl::KdlValue) -> String {
    match value {
        kdl::KdlValue::String(s) => s.clone(),
        kdl::KdlValue::Integer(i) => i.to_string(),
        kdl::KdlValue::Bool(b) => b.to_string(),
        kdl::KdlValue::Float(f) => f.to_string(),
        kdl::KdlValue::Null => String::new(),
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
        assert!(task.args.get("register").is_none());
    }

    #[test]
    fn test_parse_loop_variable() {
        let input = r#"
plan "test" {
    step "Format each" loop="${disks}" {
        disk "${item}" fs="ext4"
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
        assert!(task.args.get("register").is_none());
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
    fn test_parse_loop_literal() {
        let input = r#"
plan "test" {
    step "Iterate" loop="alpha" {
        shell "echo ${item}"
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
        shell "echo ${item}"
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
        assert!(plan.vars.get("from-child").is_none());

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
        assert!(fp.vars.get("api-keys").is_none());
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
        assert!(fp.vars.get("items").is_none());
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
        let dir = std::env::temp_dir().join("glidesh_test_vars_file");
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
        let dir = std::env::temp_dir().join("glidesh_test_vars_file_override");
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
        let dir = std::env::temp_dir().join("glidesh_test_vars_file_missing");
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
        let dir = std::env::temp_dir().join("glidesh_test_dup_vars_file");
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
}
