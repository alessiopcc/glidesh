use crate::config::condition::Condition;
use crate::config::types::{
    Amount, ExecutionMode, Include, LoopSource, ParamValue, Plan, PlanItem, RunAsSpec, Step,
    TaskDef, UntilGate, VarPrompt,
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
    let mut prompts = Vec::new();
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
                    // Rejected rather than defaulted: a typo would silently run in sync mode.
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
            "vars-prompt" => {
                for prompt in parse_vars_prompt(node)? {
                    if prompts.iter().any(|p: &VarPrompt| p.name == prompt.name) {
                        return Err(GlideshError::ConfigParse {
                            message: format!(
                                "vars-prompt declares '{}' more than once",
                                prompt.name
                            ),
                        });
                    }
                    prompts.push(prompt);
                }
            }
            "step" => {
                items.push(PlanItem::Step(Box::new(parse_step(node)?)));
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
                items.push(PlanItem::Include(parse_include(node)?));
            }
            other => {
                return Err(GlideshError::ConfigParse {
                    message: format!("Unknown node in plan: '{}'", other),
                });
            }
        }
    }

    let run_as = super::parse_run_as_attrs(fp_node)?;

    let plan = Plan {
        name,
        mode,
        serial,
        max_fail,
        vars,
        structured_vars,
        vars_files,
        prompts,
        answers: HashMap::new(),
        run_as,
        items,
    };
    check_prompts_are_not_vars(&plan)?;
    Ok(plan)
}

const PROMPT_ATTRS: &[&str] = &["default", "secret"];

/// `vars-prompt { name "Question" default="…" secret=#true }`, one child per variable.
fn parse_vars_prompt(node: &kdl::KdlNode) -> Result<Vec<VarPrompt>, GlideshError> {
    let error = |message: String| GlideshError::ConfigParse { message };
    // Ignored, `vars-prompt secret=#true { … }` would read as marking every answer secret
    // while each one is still echoed and printed.
    if !node.entries().is_empty() {
        return Err(error(
            "vars-prompt takes no arguments or attributes: put default= and secret= on each \
             variable, e.g. vars-prompt { password \"Password\" secret=#true }"
                .to_string(),
        ));
    }
    let Some(children) = node.children() else {
        return Err(error(
            "vars-prompt needs a block of variables, e.g. vars-prompt { release \"Release to \
             deploy\" default=\"main\" }"
                .to_string(),
        ));
    };
    let mut prompts = Vec::new();
    for child in children.nodes() {
        let name = child.name().value().to_string();
        super::validate_user_var_name(&name)?;
        // `--var name=value` splits at the first `=` and trims the name, and a value starting
        // with `-` is read as another flag, so a name that could not be written that way
        // could never be answered without a terminal.
        if name.is_empty()
            || name.starts_with('-')
            || name.contains('=')
            || name.contains(char::is_whitespace)
        {
            return Err(error(format!(
                "vars-prompt name {name:?} cannot be given as --var name=value: use a name \
                 that does not start with '-' and has no '=' or whitespace"
            )));
        }
        let mut args = child.entries().iter().filter(|e| e.name().is_none());
        let text = match (args.next(), args.next()) {
            (Some(text), None) => text.value().as_string().map(str::to_string),
            _ => None,
        }
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            error(format!(
                "vars-prompt '{name}' needs one question to ask, e.g. {name} \"What to use for \
                 {name}\""
            ))
        })?;
        let mut default = None;
        let mut secret = false;
        for entry in child.entries() {
            let Some(attr) = entry.name() else { continue };
            match attr.value() {
                "default" => {
                    default = Some(super::kdl_value_to_string(entry.value()));
                }
                "secret" => {
                    secret = entry.value().as_bool().ok_or_else(|| {
                        error(format!(
                            "vars-prompt '{name}': secret must be #true or #false"
                        ))
                    })?;
                }
                other => {
                    return Err(error(format!(
                        "vars-prompt '{name}': unknown attribute '{other}' (expected one of: {})",
                        PROMPT_ATTRS.join(", ")
                    )));
                }
            }
        }
        if child.children().is_some() {
            return Err(error(format!(
                "vars-prompt '{name}' takes no block, only a question and attributes"
            )));
        }
        prompts.push(VarPrompt {
            name,
            text,
            default,
            secret,
        });
    }
    Ok(prompts)
}

/// A name both prompted for and set in `vars` would leave it unclear which one a run uses.
fn check_prompts_are_not_vars(plan: &Plan) -> Result<(), GlideshError> {
    match plan
        .prompts
        .iter()
        .find(|p| plan.vars.contains_key(&p.name) || plan.structured_vars.contains_key(&p.name))
    {
        Some(p) => Err(GlideshError::ConfigParse {
            message: format!(
                "'{}' is both in vars-prompt and a plan variable (vars, vars-file or an \
                 included plan's vars): remove one, or give the prompt a default instead",
                p.name
            ),
        }),
        None => Ok(()),
    }
}

fn parse_include(node: &kdl::KdlNode) -> Result<Include, GlideshError> {
    let path = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "include requires a path argument".to_string(),
        })?
        .to_string();
    // An attribute other than tags= used to be ignored, so a misspelled `tag=` would
    // quietly leave the included steps untagged.
    for entry in node.entries() {
        if let Some(key) = entry.name().map(|n| n.value()) {
            if key != "tags" {
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "include '{path}': unknown attribute '{key}' (expected one of: tags)"
                    ),
                });
            }
        }
    }
    let tags = match node.get("tags") {
        Some(value) => {
            let list = value.as_string().ok_or_else(|| GlideshError::ConfigParse {
                message: format!("include '{path}': tags= must be a string, like \"web,deploy\""),
            })?;
            super::tags::parse_list(list, &format!("include '{path}'"))?
        }
        None => Vec::new(),
    };
    Ok(Include { path, tags })
}

fn amount_error(setting: &str, min: usize, got: &str) -> GlideshError {
    GlideshError::ConfigParse {
        message: format!(
            "{setting} must be a host count of at least {min} or a percentage of \
             {min}–100%, got {got}"
        ),
    }
}

/// A host count (`2`) or a percentage (`"25%"`), at least `min`. A count may also be written
/// as a string (`"2"`).
fn parse_amount(value: &kdl::KdlValue, setting: &str, min: usize) -> Result<Amount, GlideshError> {
    if let Some(n) = value.as_integer() {
        return usize::try_from(n)
            .ok()
            .filter(|n| *n >= min)
            .map(Amount::Count)
            .ok_or_else(|| amount_error(setting, min, &n.to_string()));
    }
    match value.as_string() {
        Some(text) => parse_amount_text(text, setting, min),
        None => Err(amount_error(setting, min, &value.to_string())),
    }
}

/// [`parse_amount`] for text — also how `--serial` and `--max-fail` read their values, so the
/// command line and a plan accept exactly the same thing.
pub fn parse_amount_text(text: &str, setting: &str, min: usize) -> Result<Amount, GlideshError> {
    let fail = || amount_error(setting, min, &format!("\"{text}\""));
    match text.trim().strip_suffix('%') {
        Some(pct) => pct
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= 100 && usize::from(*p) >= min)
            .map(Amount::Percent)
            .ok_or_else(fail),
        None => text
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= min)
            .map(Amount::Count)
            .ok_or_else(fail),
    }
}

/// Recursively resolve all `include` items in a plan by loading referenced plan files
/// and inlining their steps. Each included plan's vars, inline and from its own
/// `vars-file`s, are merged into the plan's (the including plan wins on conflict).
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
        &mut plan.vars,
        &mut plan.structured_vars,
        None,
        base_dir,
        &mut seen,
        &Inherited::default(),
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

    check_prompts_are_not_vars(plan)
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

/// What the plans including an included plan layer onto every step it brings in.
#[derive(Default)]
struct Inherited {
    run_as: RunAsSpec,
    tags: Vec<String>,
}

/// `vars` and `structured` accumulate every plan's variables, the including plan's
/// inserted before its includes' so it wins. `source_dir` is `None` for the top-level plan,
/// whose steps resolve relative paths from the run's plan directory.
fn resolve_items(
    items: &[PlanItem],
    vars: &mut HashMap<String, String>,
    structured: &mut HashMap<String, Vec<HashMap<String, String>>>,
    source_dir: Option<&Path>,
    base_dir: &Path,
    seen: &mut HashSet<String>,
    inherited: &Inherited,
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
                s.run_as = s.run_as.clone().merge_over(&inherited.run_as);
                s.source_dir = source_dir.map(Path::to_path_buf);
                s.tags = super::tags::merge(&s.tags, &inherited.tags);
                result.push(PlanItem::Step(s));
            }
            PlanItem::Include(Include { path, tags }) => {
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
                // Rejected rather than ignored: silently not asking would run with the
                // variable undefined.
                if let Some(prompt) = included.prompts.first() {
                    return Err(GlideshError::ConfigParse {
                        message: format!(
                            "included plan '{}' ({}) declares vars-prompt '{}': only the plan \
                             you run may ask for variables, so move the vars-prompt block there",
                            included.name,
                            resolved_path.display(),
                            prompt.name
                        ),
                    });
                }
                if !seen.insert(included.name.clone()) {
                    return Err(GlideshError::ConfigParse {
                        message: format!(
                            "Circular include detected: plan '{}' already included",
                            included.name
                        ),
                    });
                }
                let child_base = resolved_path.parent().unwrap_or(base_dir);
                let mut own_vars = included.vars;
                let mut own_structured = included.structured_vars;
                resolve_vars_files(
                    &included.vars_files,
                    &mut own_vars,
                    &mut own_structured,
                    child_base,
                )?;
                for (k, v) in own_vars {
                    vars.entry(k).or_insert(v);
                }
                for (k, v) in own_structured {
                    structured.entry(k).or_insert(v);
                }

                // The included plan's plan-level run-as governs its own steps,
                // layered under anything inherited from the including plan(s).
                // Its steps also carry the include's tags, then any from further out.
                let child = Inherited {
                    run_as: included.run_as.clone().merge_over(&inherited.run_as),
                    tags: super::tags::merge(tags, &inherited.tags),
                };
                let child_dir =
                    std::fs::canonicalize(child_base).unwrap_or_else(|_| child_base.to_path_buf());
                let child_items = resolve_items(
                    &included.items,
                    vars,
                    structured,
                    Some(&child_dir),
                    child_base,
                    seen,
                    &child,
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

    let until = parse_until(node, &name)?;

    let tags = match node.get("tags") {
        Some(value) => {
            let list = value.as_string().ok_or_else(|| GlideshError::ConfigParse {
                message: format!("step '{name}': tags= must be a string, like \"web,deploy\""),
            })?;
            super::tags::parse_list(list, &format!("step '{name}'"))?
        }
        None => Vec::new(),
    };

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
    let mut rescue = None;
    let mut always = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let section = match child.name().value() {
                "rescue" => &mut rescue,
                "always" => &mut always,
                _ => {
                    tasks.push(parse_task(child)?);
                    continue;
                }
            };
            let kind = child.name().value();
            if section.is_some() {
                return Err(GlideshError::ConfigParse {
                    message: format!("step '{name}': more than one {kind} block"),
                });
            }
            *section = Some(parse_section(child, &name, kind)?);
        }
    }

    // No failure exists yet where the step's own work runs.
    if let Some(var) = find_reference(
        is_error_var,
        &tasks,
        when.as_ref(),
        until.as_ref(),
        &loop_source,
    ) {
        return Err(GlideshError::ConfigParse {
            message: format!(
                "step '{name}': only rescue and always tasks can use ${{{var}}}, which describes \
                 the step's failure"
            ),
        });
    }

    let run_as = super::parse_run_as_attrs(node)?;

    Ok(Step {
        name,
        tasks,
        loop_source,
        subscribe,
        run_as,
        when,
        tags,
        until,
        rescue: rescue.unwrap_or_default(),
        always: always.unwrap_or_default(),
        source_dir: None,
    })
}

/// The tasks of a step's `rescue { }` or `always { }` block.
fn parse_section(
    node: &kdl::KdlNode,
    step: &str,
    kind: &str,
) -> Result<Vec<TaskDef>, GlideshError> {
    let err = |message: String| GlideshError::ConfigParse {
        message: format!("step '{step}': {message}"),
    };
    if !node.entries().is_empty() {
        return Err(err(format!(
            "{kind} takes no arguments or attributes, only a block of tasks: {kind} {{ ... }}"
        )));
    }
    let children = node.children().map(|c| c.nodes()).unwrap_or_default();
    if children.is_empty() {
        return Err(err(format!("{kind} needs at least one task")));
    }
    let tasks = children
        .iter()
        .map(|child| match child.name().value() {
            nested @ ("rescue" | "always") => Err(err(format!(
                "{nested} cannot go inside {kind}; put it directly in the step"
            ))),
            _ => parse_task(child),
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The block runs once, after the loop, when no item is bound.
    if let Some(var) = find_reference(is_item_var, &tasks, None, None, &None) {
        return Err(err(format!(
            "{kind} cannot use ${{{var}}}: it runs once per step, after the loop"
        )));
    }
    Ok(tasks)
}

/// The first variable `is_var` accepts that the given tasks, `when=`, `until=` or `loop=`
/// reference.
fn find_reference(
    is_var: fn(&str) -> bool,
    tasks: &[TaskDef],
    when: Option<&Condition>,
    until: Option<&UntilGate>,
    loop_source: &Option<LoopSource>,
) -> Option<String> {
    let in_text = |text: &str| {
        crate::config::template::tokens(text)
            .into_iter()
            .find_map(|(_, token)| match token {
                crate::config::template::Token::Var(name) if is_var(name) => Some(name.to_string()),
                _ => None,
            })
    };
    let in_condition = |cond: Option<&Condition>| {
        cond.and_then(|c| c.variables().find(|v| is_var(v)).map(str::to_string))
    };
    let in_args = |task: &TaskDef| {
        task.args.values().find_map(|value| match value {
            ParamValue::String(s) => in_text(s),
            ParamValue::List(items) => items.iter().find_map(|s| in_text(s)),
            ParamValue::Map(map) => map.values().find_map(|s| in_text(s)),
            _ => None,
        })
    };
    let in_loop = match loop_source {
        Some(LoopSource::Variable(var)) if is_var(var) => Some(var.clone()),
        _ => None,
    };
    in_condition(when)
        .or_else(|| until.and_then(|u| in_text(&u.command)))
        .or(in_loop)
        .or_else(|| {
            tasks.iter().find_map(|task| {
                in_text(&task.resource)
                    .or_else(|| in_args(task))
                    .or_else(|| in_condition(task.when.as_ref()))
            })
        })
}

pub(crate) fn is_error_var(name: &str) -> bool {
    name == "@error" || name.starts_with("@error.")
}

pub(crate) const STEP_ATTRS: &[&str] = &[
    "loop",
    "subscribe",
    "when",
    "tags",
    "until",
    "until-timeout",
    "until-interval",
    "run-as",
    "run-as-method",
];

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

fn parse_until(node: &kdl::KdlNode, step: &str) -> Result<Option<UntilGate>, GlideshError> {
    let err = |message: String| GlideshError::ConfigParse {
        message: format!("step '{step}': {message}"),
    };
    let seconds = |key: &str, default: u64| -> Result<u64, GlideshError> {
        match node.get(key) {
            None => Ok(default),
            Some(value) => value
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .filter(|n| (1..=UntilGate::MAX_SECONDS).contains(n))
                .ok_or_else(|| {
                    err(format!(
                        "{key}= must be a number of seconds from 1 to {} (7 days)",
                        UntilGate::MAX_SECONDS
                    ))
                }),
        }
    };
    let timeout = seconds("until-timeout", UntilGate::DEFAULT_TIMEOUT)?;
    let interval = seconds("until-interval", UntilGate::DEFAULT_INTERVAL)?;
    let Some(value) = node.get("until") else {
        if node.get("until-timeout").is_some() || node.get("until-interval").is_some() {
            return Err(err(
                "until-timeout= and until-interval= need an until= command".into(),
            ));
        }
        return Ok(None);
    };
    let command = value
        .as_string()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| {
            err("until= must be a command, like until=\"curl -sf localhost:8080/health\"".into())
        })?;
    if command.contains("${@item") {
        return Err(err(
            "until= cannot use ${@item}: the gate runs once, before the step's loop".into(),
        ));
    }
    Ok(Some(UntilGate {
        command: command.to_string(),
        timeout,
        interval,
    }))
}

pub(crate) fn is_item_var(name: &str) -> bool {
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
            } else if key == "tags" && !module.starts_with("external.") {
                // No built-in module reads it, so the task would run under every --tags
                // while looking selected. A plugin may take its own `tags` parameter.
                return Err(GlideshError::ConfigParse {
                    message: format!(
                        "{module} '{resource}': tags= goes on the step, not on a task; \
                         move the task to its own step to tag it"
                    ),
                });
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
                // `- "a" - "b"` on one line is one `-` node holding the rest as values:
                // taking the first would drop the others unseen — an `exclude` pattern, and
                // with `prune` the files it was meant to keep.
                let mut list = Vec::new();
                for item in child.children().unwrap().nodes() {
                    match item.entries() {
                        [one] if one.name().is_none() => {
                            list.push(super::kdl_value_to_string(one.value()))
                        }
                        _ => {
                            return Err(GlideshError::ConfigParse {
                                message: format!(
                                    "{module} '{resource}': {key}: each `-` item takes one \
                                     value; put items on their own lines, or separate them \
                                     with `;` (`- \"a\"; - \"b\"`)"
                                ),
                            });
                        }
                    }
                }
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
                let positional: Vec<_> = child
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .collect();
                // Several values are a list (`groups "docker" "sudo"`): keeping only the
                // first would silently drop the rest.
                let value = match positional.as_slice() {
                    [] => ParamValue::String(String::new()),
                    [one] => kdl_value_to_param(one.value()),
                    many => ParamValue::List(
                        many.iter()
                            .map(|e| super::kdl_value_to_string(e.value()))
                            .collect(),
                    ),
                };
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

    /// `- "a" - "b"` on one line is one item holding three values; keeping the first used
    /// to drop the rest unseen.
    #[test]
    fn a_list_item_with_several_values_is_rejected() {
        let err = plan_err(
            r#"step "s" { file "/srv/app" src="site" recurse=#true { exclude { - ".git" - "*.log" } } }"#,
        );
        assert!(
            err.contains("exclude: each `-` item takes one value"),
            "{err}"
        );
        let ok = parse_plan(
            r#"plan "p" { step "s" { container "c" image="x" { ports { - "80:80"; - 8443 } } } }"#,
        )
        .unwrap();
        let ports = ok.steps()[0].tasks[0].args["ports"]
            .as_list()
            .unwrap()
            .to_vec();
        assert_eq!(ports, ["80:80", "8443"], "a number is kept, as text");
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
        assert!(
            matches!(&fp.items[1], PlanItem::Include(i) if i.path == "common/security.kdl" && i.tags.is_empty())
        );
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

    fn prompts(body: &str) -> Result<Plan, GlideshError> {
        parse_plan(&format!("plan \"p\" {{\n{body}\n}}"))
    }

    #[test]
    fn vars_prompt_declares_questions_defaults_and_secrets() {
        let plan = prompts(
            "vars-prompt {\n release \"Release to deploy\" default=\"main\"\n \
             db-password \"Database password\" secret=#true\n port \"Port\" default=8080\n}",
        )
        .unwrap();
        assert_eq!(
            plan.prompts,
            [
                VarPrompt {
                    name: "release".into(),
                    text: "Release to deploy".into(),
                    default: Some("main".into()),
                    secret: false,
                },
                VarPrompt {
                    name: "db-password".into(),
                    text: "Database password".into(),
                    default: None,
                    secret: true,
                },
                VarPrompt {
                    name: "port".into(),
                    text: "Port".into(),
                    default: Some("8080".into()),
                    secret: false,
                },
            ]
        );
    }

    #[test]
    fn malformed_vars_prompts_are_rejected() {
        for (body, needle) in [
            ("vars-prompt", "needs a block"),
            (
                "vars-prompt secret=#true {\n password \"Password\"\n}",
                "takes no arguments or attributes",
            ),
            (
                "vars-prompt \"x\" {\n release \"A\"\n}",
                "takes no arguments or attributes",
            ),
            ("vars-prompt {\n release\n}", "needs one question"),
            ("vars-prompt {\n release \"\"\n}", "needs one question"),
            ("vars-prompt {\n release 5\n}", "needs one question"),
            (
                "vars-prompt {\n release \"A\" \"B\"\n}",
                "needs one question",
            ),
            (
                "vars-prompt {\n release \"A\" secret=\"yes\"\n}",
                "#true or #false",
            ),
            (
                "vars-prompt {\n release \"A\" hidden=#true\n}",
                "unknown attribute 'hidden'",
            ),
            ("vars-prompt {\n release \"A\" { x 1 }\n}", "takes no block"),
            (
                "vars-prompt {\n release \"A\"\n release \"B\"\n}",
                "more than once",
            ),
            (
                "vars-prompt {\n release \"A\"\n}\nvars-prompt {\n release \"B\"\n}",
                "more than once",
            ),
            ("vars-prompt {\n \"@host.name\" \"A\"\n}", "reserved"),
            (
                "vars-prompt {\n \"release=tag\" \"A\"\n}",
                "cannot be given as --var",
            ),
            ("vars-prompt {\n \"\" \"A\"\n}", "cannot be given as --var"),
            (
                "vars-prompt {\n \"-rel\" \"A\"\n}",
                "cannot be given as --var",
            ),
            (
                "vars-prompt {\n \" release\" \"A\"\n}",
                "cannot be given as --var",
            ),
            (
                "vars-prompt {\n \"my release\" \"A\"\n}",
                "cannot be given as --var",
            ),
            (
                "vars {\n release \"v1\"\n}\nvars-prompt {\n release \"A\"\n}",
                "both in vars-prompt and a plan variable",
            ),
        ] {
            let err = match prompts(body) {
                Ok(_) => panic!("accepted: {body}"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(needle), "{body}: {err}");
        }
    }

    /// Ignoring it, as an included plan's rollout settings are, would run the included steps
    /// with the variable silently undefined.
    #[test]
    fn an_included_plans_vars_prompt_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("child.kdl"),
            "plan \"child\" {\n    vars-prompt {\n        release \"Release\"\n    }\n}",
        )
        .unwrap();
        let mut plan = parse_plan("plan \"parent\" {\n    include \"child.kdl\"\n}").unwrap();
        let err = resolve_includes(&mut plan, dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("included plan 'child'") && err.contains("vars-prompt 'release'"),
            "{err}"
        );
    }

    #[test]
    fn a_prompted_name_set_by_a_vars_file_or_an_include_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vars.kdl"), "release \"v1\"\n").unwrap();
        std::fs::write(
            dir.path().join("child.kdl"),
            "plan \"child\" {\n    vars {\n        region \"eu\"\n    }\n}",
        )
        .unwrap();
        for body in [
            "vars-file \"vars.kdl\"\n vars-prompt {\n release \"Release\"\n }",
            "include \"child.kdl\"\n vars-prompt {\n region \"Region\"\n }",
        ] {
            let mut plan = prompts(body).unwrap();
            let err = resolve_includes(&mut plan, dir.path())
                .unwrap_err()
                .to_string();
            assert!(err.contains("both in vars-prompt"), "{body}: {err}");
        }
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
        assert_eq!(plan.vars.get("shared").unwrap(), "parent-version");
        assert_eq!(plan.vars.get("from-child").unwrap(), "child-value");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes `files` (relative path, content) under a fresh directory and resolves
    /// `main.kdl` there.
    fn resolve_tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Plan) {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let main = std::fs::read_to_string(dir.path().join("main.kdl")).unwrap();
        let mut plan = parse_plan(&main).unwrap();
        resolve_includes(&mut plan, dir.path()).unwrap();
        (dir, plan)
    }

    #[test]
    fn included_vars_files_load_from_their_own_directory_and_the_includer_wins() {
        let (_dir, plan) = resolve_tree(&[
            (
                "main.kdl",
                r#"plan "main" {
                    vars { shared "main" }
                    include "roles/web/plan.kdl"
                }"#,
            ),
            (
                "roles/web/plan.kdl",
                r#"plan "web" {
                    vars-file "defaults.kdl"
                    vars { inline "web" }
                    include "nested/plan.kdl"
                    step "Web" { shell "true" }
                }"#,
            ),
            (
                "roles/web/defaults.kdl",
                "shared \"web-file\"\nfrom-file \"web-file\"\ninline \"web-file\"\n",
            ),
            (
                "roles/web/nested/plan.kdl",
                r#"plan "nested" {
                    vars { from-file "nested"; only-nested "nested"; }
                    step "Nested" { shell "true" }
                }"#,
            ),
        ]);
        assert_eq!(plan.vars["shared"], "main");
        assert_eq!(plan.vars["inline"], "web");
        assert_eq!(plan.vars["from-file"], "web-file");
        assert_eq!(plan.vars["only-nested"], "nested");
    }

    #[test]
    fn a_child_node_with_several_values_is_a_list() {
        let plan = parse_plan(
            r#"plan "p" {
                step "Users" {
                    user "deploy" {
                        groups "docker" "sudo"
                        shell "/bin/bash"
                    }
                }
            }"#,
        )
        .unwrap();
        let args = &plan.steps()[0].tasks[0].args;
        assert_eq!(
            args["groups"].as_list(),
            Some(&["docker".to_string(), "sudo".to_string()][..])
        );
        assert_eq!(args["shell"].as_str(), Some("/bin/bash"));
    }

    #[test]
    fn included_steps_remember_the_directory_of_their_own_plan() {
        let (dir, plan) = resolve_tree(&[
            (
                "main.kdl",
                r#"plan "main" {
                    step "Top" { shell "true" }
                    include "roles/web/plan.kdl"
                }"#,
            ),
            (
                "roles/web/plan.kdl",
                r#"plan "web" {
                    step "Web" { shell "true" }
                    include "../db/plan.kdl"
                }"#,
            ),
            (
                "roles/db/plan.kdl",
                r#"plan "db" { step "Db" { shell "true" } }"#,
            ),
        ]);
        let canon = |p: &str| std::fs::canonicalize(dir.path().join(p)).unwrap();
        let steps = plan.steps();
        assert_eq!(steps[0].source_dir, None);
        assert_eq!(steps[0].base_dir(dir.path()), dir.path());
        assert_eq!(steps[1].source_dir, Some(canon("roles/web")));
        assert_eq!(steps[2].source_dir, Some(canon("roles/db")));
    }

    #[test]
    fn included_steps_keep_their_tags() {
        let (_dir, plan) = resolve_tree(&[
            ("main.kdl", r#"plan "main" { include "roles/web.kdl" }"#),
            (
                "roles/web.kdl",
                r#"plan "web" { step "Web" tags="web,deploy" { shell "true" } }"#,
            ),
        ]);
        assert_eq!(plan.steps()[0].tags, ["web", "deploy"]);
    }

    #[test]
    fn an_include_takes_a_tag_list() {
        let plan = parse_plan(r#"plan "p" { include "web.kdl" tags="web, deploy,web" }"#).unwrap();
        let PlanItem::Include(include) = &plan.items[0] else {
            panic!("expected an include");
        };
        assert_eq!(
            *include,
            (Include {
                path: "web.kdl".to_string(),
                tags: vec!["web".to_string(), "deploy".to_string()],
            })
        );
    }

    #[test]
    fn malformed_include_tags_or_an_unknown_include_attribute_fail_to_parse() {
        let err = plan_err(r#"include "web.kdl" tags="web,,db""#);
        assert!(
            err.contains("include 'web.kdl': invalid tag list 'web,,db'"),
            "{err}"
        );
        let err = plan_err(r#"include "web.kdl" tags=#true"#);
        assert!(
            err.contains("include 'web.kdl': tags= must be a string"),
            "{err}"
        );
        let err = plan_err(r#"include "web.kdl" tag="web""#);
        assert!(
            err.contains("include 'web.kdl': unknown attribute 'tag' (expected one of: tags)"),
            "{err}"
        );
    }

    /// An include's tags follow each step's own, then those of includes further out,
    /// each tag once.
    #[test]
    fn include_tags_reach_every_step_brought_in_including_nested_ones() {
        let (_dir, plan) = resolve_tree(&[
            (
                "main.kdl",
                r#"plan "main" {
                    step "Top" tags="top" { shell "true" }
                    include "roles/web.kdl" tags="web,deploy"
                }"#,
            ),
            (
                "roles/web.kdl",
                r#"plan "web" {
                    step "Web" tags="deploy,nginx" { shell "true" }
                    include "common.kdl" tags="base"
                }"#,
            ),
            (
                "roles/common.kdl",
                r#"plan "common" {
                    step "Common" { shell "true" }
                    step "Common web" tags="web" { shell "true" }
                }"#,
            ),
        ]);
        let tags: Vec<(&str, Vec<&str>)> = plan
            .steps()
            .iter()
            .map(|s| (s.name.as_str(), s.tags.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            tags,
            [
                ("Top", vec!["top"]),
                ("Web", vec!["deploy", "nginx", "web"]),
                ("Common", vec!["base", "web", "deploy"]),
                ("Common web", vec!["web", "base", "deploy"]),
            ]
        );
    }

    #[test]
    fn tags_selects_the_steps_an_include_tags() {
        let (_dir, plan) = resolve_tree(&[
            (
                "main.kdl",
                r#"plan "main" {
                    step "Top" { shell "true" }
                    include "db.kdl" tags="db"
                }"#,
            ),
            ("db.kdl", r#"plan "db" { step "Db" { shell "true" } }"#),
        ]);
        let filter = super::super::tags::TagFilter::from_args(Some("db"), None).unwrap();
        filter.check_known([&plan].into_iter()).unwrap();
        let steps = plan.steps();
        assert!(filter.excludes(&steps[0].tags).is_some());
        assert_eq!(filter.excludes(&steps[1].tags), None);
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

    #[test]
    fn until_has_defaults_and_takes_its_timings_as_attributes() {
        let step = one_step(r#"step "s" until="curl -sf localhost" { shell "echo" }"#);
        assert_eq!(
            step.until,
            Some(UntilGate {
                command: "curl -sf localhost".into(),
                timeout: UntilGate::DEFAULT_TIMEOUT,
                interval: UntilGate::DEFAULT_INTERVAL,
            })
        );
        let step = one_step(r#"step "s" until="true" until-timeout=600 until-interval=5 {}"#);
        let gate = step.until.unwrap();
        assert_eq!((gate.timeout, gate.interval), (600, 5));
    }

    /// A gate with no tasks is a pure barrier.
    #[test]
    fn a_step_may_be_only_a_gate() {
        let step = one_step(r#"step "Wait" until="test -e /tmp/ready""#);
        assert!(step.until.is_some() && step.tasks.is_empty());
    }

    #[test]
    fn malformed_until_settings_fail_to_parse() {
        for (attrs, expect) in [
            (r#"until="""#, "must be a command"),
            (r#"until=#true"#, "must be a command"),
            (
                r#"until="x" until-timeout=0"#,
                "until-timeout= must be a number",
            ),
            (
                r#"until="x" until-interval=-1"#,
                "until-interval= must be a number",
            ),
            (
                r#"until="x" until-timeout="60""#,
                "until-timeout= must be a number",
            ),
            (r#"until="x" until-timeout=604801"#, "from 1 to 604800"),
            (
                r#"until="x" until-interval=18446744073709551615"#,
                "from 1 to 604800",
            ),
            (r#"until-timeout=60"#, "need an until= command"),
            (
                r#"loop="${xs}" until="test ${@item}""#,
                "cannot use ${@item}",
            ),
        ] {
            let err = plan_err(&format!(r#"step "s" {attrs} {{ shell "echo" }}"#));
            assert!(err.contains(expect), "{attrs}: {err}");
        }
    }

    #[test]
    fn tags_are_a_comma_list_on_the_step() {
        let step = one_step(r#"step "s" tags="web, deploy" { shell "echo" }"#);
        assert_eq!(step.tags, ["web", "deploy"]);
        assert!(one_step(r#"step "s" { shell "echo" }"#).tags.is_empty());
    }

    #[test]
    fn a_builtin_task_cannot_be_tagged_but_a_plugin_may_take_tags() {
        let err = plan_err(r#"step "s" { shell "echo" tags="web" }"#);
        assert!(err.contains("tags= goes on the step"), "{err}");
        let step = one_step(r#"step "s" { external "acme/vm" "web" tags="prod" }"#);
        assert!(step.tasks[0].args.contains_key("tags"));
    }

    #[test]
    fn malformed_tags_fail_to_parse() {
        let err = plan_err(r#"step "s" tags="web,,db" { shell "echo" }"#);
        assert!(
            err.contains("step 's'") && err.contains("invalid tag list"),
            "{err}"
        );
        let err = plan_err(r#"step "s" tags=#true { shell "echo" }"#);
        assert!(err.contains("must be a string"), "{err}");
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
            step "s" loop="${xs}" subscribe="a" when="${x}" tags="t" until="true" until-timeout=5 until-interval=1 run-as="root" run-as-method="sudo" {
                shell "echo"
            }"#,
        );
    }

    #[test]
    fn rescue_and_always_hold_their_own_tasks() {
        let step = one_step(
            r#"step "s" {
                shell "deploy.sh"
                rescue {
                    shell "rollback.sh" when="defined ${@error.task}"
                    shell "echo ${@error.msg}" register="why"
                }
                always { shell "rm -f /tmp/lock" }
                shell "verify.sh"
            }"#,
        );
        let resources = |tasks: &[TaskDef]| -> Vec<String> {
            tasks.iter().map(|t| t.resource.clone()).collect()
        };
        assert_eq!(resources(&step.tasks), ["deploy.sh", "verify.sh"]);
        assert_eq!(
            resources(&step.rescue),
            ["rollback.sh", "echo ${@error.msg}"]
        );
        assert_eq!(resources(&step.always), ["rm -f /tmp/lock"]);
        assert_eq!(step.all_tasks().count(), 5);
    }

    #[test]
    fn rescue_and_always_cannot_read_the_loop_item() {
        for (kind, var) in [("rescue", "@item"), ("always", "@item.name")] {
            let err = plan_err(&format!(
                r#"step "s" loop="${{xs}}" {{ shell "a"; {kind} {{ shell "echo ${{{var}}}" }} }}"#
            ));
            assert!(
                err.contains(&format!("{kind} cannot use ${{{var}}}")),
                "{err}"
            );
        }
        let err = plan_err(
            r#"step "s" loop="${xs}" { shell "a"; rescue { shell "b" when="${@item} == x" } }"#,
        );
        assert!(err.contains("rescue cannot use ${@item}"), "{err}");
    }

    #[test]
    fn a_step_without_rescue_or_always_has_none() {
        let step = one_step(r#"step "s" { shell "echo" }"#);
        assert!(step.rescue.is_empty() && step.always.is_empty());
    }

    #[test]
    fn malformed_rescue_and_always_blocks_fail_to_parse() {
        for (body, expected) in [
            (
                r#"step "s" { shell "a"; rescue { shell "b" }; rescue { shell "c" } }"#,
                "more than one rescue block",
            ),
            (
                r#"step "s" { shell "a"; always }"#,
                "always needs at least one task",
            ),
            (
                r#"step "s" { shell "a"; rescue { } }"#,
                "rescue needs at least one task",
            ),
            (
                r#"step "s" { shell "a"; rescue "x" { shell "b" } }"#,
                "rescue takes no arguments or attributes",
            ),
            (
                r#"step "s" { shell "a"; rescue when="${x}" { shell "b" } }"#,
                "rescue takes no arguments or attributes",
            ),
            (
                r#"step "s" { shell "a"; rescue { always { shell "b" } } }"#,
                "always cannot go inside rescue",
            ),
        ] {
            let err = plan_err(body);
            assert!(err.contains(expected), "{body}: {err}");
        }
    }

    /// No failure exists yet where the step's own work runs, so the reference could only
    /// fail at run time.
    #[test]
    fn only_rescue_and_always_can_read_the_error() {
        for body in [
            r#"step "s" { shell "echo ${@error.msg}" }"#,
            r#"step "s" { shell "echo" when="defined ${@error.msg}" }"#,
            r#"step "s" { shell { cmd "a" "echo ${@error.task}" } }"#,
            r#"step "s" { shell "a" { environment { WHY "${@error.msg}" } } }"#,
            r#"step "s" when="defined ${@error.msg}" { shell "a" }"#,
            r#"step "s" until="test -n '${@error.msg}'" { shell "a" }"#,
            r#"step "s" loop="${@error.msg}" { shell "a" }"#,
        ] {
            let err = plan_err(body);
            assert!(
                err.contains("only rescue and always tasks can use ${@error."),
                "{body}: {err}"
            );
        }
        one_step(r#"step "s" { shell "a"; always { shell "echo" when="defined ${@error.msg}" } }"#);
        one_step(r#"step "s" { shell "echo $${@error.msg} is text" }"#);
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
