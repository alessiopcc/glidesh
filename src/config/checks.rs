//! Checks `glidesh validate` runs on a resolved plan without contacting any host.

use crate::config::condition::{Condition, Outcome, Scope};
use crate::config::plan::{is_error_var, is_item_var};
use crate::config::prompts::closest;
use crate::config::template::interpolate;
use crate::config::template::{
    Token, bound_field, defined_references, literal_hint, structure_error, tokens,
};
use crate::config::types::{LoopSource, ParamValue, Plan, TaskDef};
use crate::modules::file_tree;
use crate::secrets::token::contains_secret_token;
use std::collections::{BTreeSet, HashMap};
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

/// The files a `file` task uploads from `resolved`: itself, or for a directory every file
/// under it that the task's `exclude` keeps, and the first directory that could not be read,
/// which a recursive upload fails on. An `exclude` that cannot be read is
/// [`file_option_problems`]' to report; the walk then leaves nothing out.
///
/// A run interpolates `exclude` first: a pattern the plan's `vars` resolve is read as they
/// do, and a tree with one only a host can resolve is not walked — which files it uploads is
/// not known yet.
fn uploaded_files(
    task: &TaskDef,
    resolved: &Path,
    vars: &HashMap<String, String>,
) -> (Vec<PathBuf>, Option<String>) {
    if !resolved.is_dir() {
        return (vec![resolved.to_path_buf()], None);
    }
    let patterns: Vec<String> = match task.args.get("exclude") {
        Some(ParamValue::List(items)) => items.clone(),
        Some(ParamValue::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    let mut interpolated = Vec::with_capacity(patterns.len());
    for pattern in &patterns {
        match interpolate(pattern, vars) {
            Ok(pattern) => interpolated.push(pattern),
            Err(_) => return (Vec::new(), None),
        }
    }
    let exclude = file_tree::exclude_of(&HashMap::from([(
        "exclude".to_string(),
        ParamValue::List(interpolated),
    )]));
    match file_tree::walk(resolved, &exclude) {
        Ok(tree) => (tree.files.iter().map(|f| resolved.join(f)).collect(), None),
        Err(unreadable) => (Vec::new(), Some(unreadable)),
    }
}

/// `file` parameters a run would reject: a recursive-only one without `recurse=#true`, a
/// value of the wrong kind, an `exclude` pattern that means nothing, a `prune` destination
/// too close to `/`.
pub fn file_option_problems(plan: &Plan) -> Vec<String> {
    let mut problems = Vec::new();
    for step in plan.steps() {
        for task in step.all_tasks().filter(|t| t.module == "file") {
            let recurse = matches!(task.args.get("recurse"), Some(ParamValue::Bool(true)));
            // A run interpolates first: what the plan's vars resolve is checked as the run will
            // see it; what needs a host or a prompted answer is left to the run.
            let dest = interpolate(&task.resource, &plan.vars).ok();
            let mut args = task.args.clone();
            for key in ["mode", "file-mode", "dir-mode", "owner", "group"] {
                if let Some(ParamValue::String(value)) = args.get(key) {
                    match interpolate(value, &plan.vars) {
                        Ok(value) => args.insert(key.to_string(), ParamValue::String(value)),
                        Err(_) => args.remove(key),
                    };
                }
            }
            let patterns = match args.get("exclude") {
                Some(ParamValue::List(items)) => Some(items.clone()),
                Some(ParamValue::String(one)) => Some(vec![one.clone()]),
                _ => None,
            };
            if let Some(patterns) = patterns {
                let resolved: Vec<String> = patterns
                    .iter()
                    .filter_map(|p| interpolate(p, &plan.vars).ok())
                    .collect();
                args.insert("exclude".to_string(), ParamValue::List(resolved));
            }
            if let Err(problem) = file_tree::options(&args, dest.as_deref(), recurse) {
                problems.push(format!(
                    "step '{}': file '{}': {}",
                    step.name, task.resource, problem
                ));
            }
        }
    }
    problems
}

/// Warnings for `file` uploads without `template #true` whose content holds what only a
/// template expands, and so would ship verbatim: a `${for}` loop, a name `is_defined` accepts
/// or the plan registers, or an undefined name written as only glidesh writes names
/// ([`glidesh_style`]), which is likely mistyped, with the closest of `known` suggested. Any
/// other undefined name is left alone, even one close to a defined name: a shell script's
/// `${HOME}` is no glidesh variable, whatever `HOMES` the plan defines.
pub fn literal_reference_warnings(
    plan: &Plan,
    plan_dir: &Path,
    is_defined: impl Fn(&str) -> bool,
    known: &[&str],
) -> Vec<String> {
    let registered = registered(plan);
    let is_defined = |name: &str| is_defined(name) || registered.contains(name);
    let known: Vec<&str> = known
        .iter()
        .copied()
        .chain(registered.iter().copied())
        .collect();
    let mut warnings = Vec::new();
    for step in plan.steps() {
        for task in step.all_tasks() {
            let templated = matches!(task.args.get("template"), Some(ParamValue::Bool(true)));
            let Some((src, resolved)) =
                local_source(task, step.base_dir(plan_dir)).filter(|_| !templated)
            else {
                continue;
            };
            let (files, _) = uploaded_files(task, &resolved, &plan.vars);
            for file in files {
                let Ok(content) = std::fs::read(&file) else {
                    continue;
                };
                let shown = shown_source(&src, &resolved, &file);
                let loops = std::str::from_utf8(&content)
                    .map(loop_reads)
                    .unwrap_or_default();
                if loops.any {
                    warnings.push(format!(
                        "step '{}': {} contains a ${{for}} loop but is uploaded as-is, because \
                         `template` is not set; add `template #true` to expand it",
                        step.name, shown
                    ));
                }
                let names = defined_references(&content, is_defined);
                if !names.is_empty() {
                    let refs: Vec<String> = names.iter().map(|n| format!("${{{n}}}")).collect();
                    warnings.push(format!(
                        "step '{}': {} contains {} but is uploaded as-is, because `template` is \
                         not set; add `template #true` to substitute",
                        step.name,
                        shown,
                        refs.join(", ")
                    ));
                }
                let typos: Vec<String> = defined_references(&content, |name| !is_defined(name))
                    .iter()
                    .filter(|name| !loops.bound_only.contains(name.as_str()) && glidesh_style(name))
                    .map(|name| match closest(name, known.iter().copied()) {
                        Some(meant) => format!("${{{name}}} (did you mean ${{{meant}}}?)"),
                        None => format!("${{{name}}}"),
                    })
                    .collect();
                if !typos.is_empty() {
                    warnings.push(format!(
                        "step '{}': {} contains undefined {}; it is uploaded as-is, because \
                         `template` is not set, so it ships literally",
                        step.name,
                        shown,
                        typos.join(", ")
                    ));
                }
            }
        }
    }
    warnings
}

/// What a file's `${for}` loops read, for a file that is not a template.
#[derive(Default)]
struct LoopReads<'a> {
    /// The file has a `${for}`.
    any: bool,
    /// `${binding.field}` names read only inside a loop over `binding`: a template would
    /// fill them, so they are no mistyped variable. One also read outside is.
    bound_only: std::collections::HashSet<&'a str>,
}

fn loop_reads(text: &str) -> LoopReads<'_> {
    let mut reads = LoopReads::default();
    let mut unbound = std::collections::HashSet::new();
    let mut open: Vec<&str> = Vec::new();
    for (_, token) in tokens(text) {
        match token {
            Token::For { binding, .. } => {
                reads.any = true;
                open.push(binding);
            }
            Token::EndFor => {
                open.pop();
            }
            Token::Var(name) if open.iter().any(|b| bound_field(name, b).is_some()) => {
                reads.bound_only.insert(name);
            }
            Token::Var(name) => {
                unbound.insert(name);
            }
            _ => {}
        }
    }
    reads.bound_only.retain(|name| !unbound.contains(name));
    reads
}

/// Whether `name` is written as glidesh names are and a shell's are not: lowercase, with a
/// `-` or `.`, or a leading `@`. No shell variable name holds any of these; the shell forms
/// that do — `${VAR-default}`, `${HOME:-/root}`, `${VAR-}`, `${1-x}`, `${@}` — have an
/// uppercase name, a `:` or `/`, an empty default, a leading digit, or nothing after the
/// `@`. A lowercase default such as `${foo-bar}` is the one shell form it still matches.
fn glidesh_style(name: &str) -> bool {
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.@".contains(c));
    let marked = name.contains(['-', '.']) || (name.starts_with('@') && name.len() > 1);
    chars_ok && marked && !name.ends_with('-') && !name.starts_with(|c: char| c.is_ascii_digit())
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
                let (files, _) = uploaded_files(task, &resolved, &plan.vars);
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
    /// loop over a list nothing defines, a field the list's items lack.
    pub problems: Vec<String>,
    /// `${name}` references nothing defines — unless an inventory `validate` was not
    /// given does.
    pub undefined: Vec<String>,
    /// `${name}` references only some of the plan's hosts define.
    pub partial: Vec<String>,
    /// `$${name}` whose `name` is defined: see [`old_escape_warning`].
    pub warnings: Vec<String>,
}

/// A list a template can loop over, as `validate` knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum List {
    /// Its items are not known here: a secrets-file list.
    Unknown,
    /// `@group.*` without an inventory: the group may not exist, and its items are not known.
    Unresolved,
    Items(Vec<HashMap<String, String>>),
}

impl List {
    pub fn of(items: &[HashMap<String, String>]) -> Self {
        List::Items(items.to_vec())
    }

    /// Why reading `field` of each item fails the render, or `None` when it does not. An
    /// empty list renders nothing, so no field of it is ever read.
    fn missing(&self, collection: &str, field: &str) -> Option<String> {
        let List::Items(items) = self else {
            return None;
        };
        let lacking = items
            .iter()
            .filter(|item| !item.contains_key(field))
            .count();
        if lacking == 0 {
            return None;
        }
        if lacking < items.len() {
            return Some(format!(
                "{lacking} of the {} items of '{collection}' have no field '{field}'",
                items.len()
            ));
        }
        let fields: BTreeSet<&str> = items
            .iter()
            .flat_map(|item| item.keys())
            .map(String::as_str)
            .collect();
        Some(format!(
            "no item of '{collection}' has a field '{field}' (available: {})",
            fields.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

/// What a `when=` can read before a run: a list, and the names `present` says are
/// (`Some(true)`) or are not (`Some(false)`) defined, all with their values unknown. Any
/// other name may or may not be. An encrypted item value is decrypted only on the host.
struct GuardScope {
    vars: HashMap<String, String>,
    collections: HashMap<String, Vec<HashMap<String, String>>>,
    unknown: std::collections::HashSet<String>,
}

impl GuardScope {
    fn new<'c>(
        conditions: impl Iterator<Item = &'c Condition>,
        present: &dyn Fn(&str) -> Option<bool>,
        list: &dyn Fn(&str) -> Option<List>,
        scalar: &dyn Fn(&str) -> bool,
    ) -> Self {
        let mut scope = GuardScope {
            vars: HashMap::new(),
            collections: HashMap::new(),
            unknown: std::collections::HashSet::new(),
        };
        for var in conditions.flat_map(Condition::variables) {
            let presence = match list(var) {
                Some(List::Unresolved) => None,
                Some(_) => {
                    scope.collections.insert(var.to_string(), Vec::new());
                    // A run compares a value of the same name instead of the list.
                    if scalar(var) {
                        scope.unknown.insert(var.to_string());
                        scope.vars.insert(var.to_string(), String::new());
                    }
                    continue;
                }
                None => present(var),
            };
            match presence {
                Some(false) => {}
                Some(true) => {
                    scope.unknown.insert(var.to_string());
                    scope.vars.insert(var.to_string(), String::new());
                }
                None => {
                    scope.unknown.insert(var.to_string());
                }
            }
        }
        scope
    }

    /// Whether `when` lets the task run, for `item` of the step's loop.
    fn reach(&self, when: Option<&Condition>, item: Option<Bound>) -> Reach {
        let Some(when) = when else {
            return Reach::Sure;
        };
        let mut vars = self.vars.clone();
        let mut unknown = self.unknown.clone();
        if let Some(item) = item {
            vars.retain(|var, _| !is_item_var(var));
            unknown.retain(|var| !is_item_var(var));
            let bound: Vec<(String, &str)> = match item {
                Bound::Fields(fields) => fields
                    .iter()
                    .map(|(field, value)| (format!("@item.{field}"), value.as_str()))
                    .collect(),
                Bound::Value(value) => vec![("@item".to_string(), value)],
            };
            for (var, value) in bound {
                if contains_secret_token(value) {
                    unknown.insert(var.clone());
                    vars.insert(var, String::new());
                } else {
                    vars.insert(var, value.to_string());
                }
            }
        }
        let scope = Scope {
            vars: &vars,
            collections: &self.collections,
            unknown: &unknown,
        };
        match when.eval(&scope) {
            Ok(Outcome::False) => Reach::Never,
            Ok(Outcome::Undetermined(_)) => Reach::Maybe,
            // A guard that cannot be evaluated fails the task rather than skip it.
            Ok(Outcome::True) | Err(_) => Reach::Sure,
        }
    }
}

/// The `${@item…}` a task's guard sees for one item of the step's loop.
#[derive(Debug, Clone, Copy)]
enum Bound<'i> {
    /// An item of a loop over a list: `${@item.<field>}`.
    Fields(&'i HashMap<String, String>),
    /// A literal item: `${@item}`.
    Value(&'i str),
}

/// The items of a step's `loop=` one of its tasks renders for. The executor evaluates the
/// task's `when=` per item before rendering, so an item it skips never reaches the template.
#[derive(Debug)]
struct Looped<'a> {
    name: &'a str,
    /// The items the task certainly renders for.
    runs: List,
    /// The items it may render for or not, depending on a value only a host knows.
    maybe: Vec<HashMap<String, String>>,
    reach: Reach,
}

impl<'a> Looped<'a> {
    /// `gate` is whether the step runs at all.
    fn new(
        name: &'a str,
        items: &List,
        gate: Reach,
        when: Option<&Condition>,
        guards: &GuardScope,
    ) -> Self {
        let List::Items(items) = items else {
            return Looped {
                name,
                runs: List::Unknown,
                maybe: Vec::new(),
                reach: gate.and(guards.reach(when, None)),
            };
        };
        let (mut runs, mut maybe) = (Vec::new(), Vec::new());
        for item in items {
            match gate.and(guards.reach(when, Some(Bound::Fields(item)))) {
                Reach::Sure => runs.push(item.clone()),
                Reach::Maybe => maybe.push(item.clone()),
                Reach::Never => {}
            }
        }
        let reach = match (runs.is_empty(), maybe.is_empty()) {
            (false, _) => Reach::Sure,
            (true, false) => Reach::Maybe,
            (true, true) => Reach::Never,
        };
        Looped {
            name,
            runs: List::Items(runs),
            maybe,
            reach,
        }
    }

    /// Every item the task may render for.
    fn all(&self) -> List {
        match &self.runs {
            List::Items(runs) => List::Items(runs.iter().chain(&self.maybe).cloned().collect()),
            _ => List::Unknown,
        }
    }
}

/// Whether a task renders its template: a `when=` that is false, or a `loop=` over no items
/// it lets through, never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Sure,
    Maybe,
    Never,
}

impl Reach {
    fn and(self, other: Reach) -> Reach {
        match (self, other) {
            (Reach::Never, _) | (_, Reach::Never) => Reach::Never,
            (Reach::Maybe, _) | (_, Reach::Maybe) => Reach::Maybe,
            (Reach::Sure, Reach::Sure) => Reach::Sure,
        }
    }

    fn or(self, other: Reach) -> Reach {
        match (self, other) {
            (Reach::Sure, _) | (_, Reach::Sure) => Reach::Sure,
            (Reach::Maybe, _) | (_, Reach::Maybe) => Reach::Maybe,
            (Reach::Never, Reach::Never) => Reach::Never,
        }
    }
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
/// must be readable, each `${for}` must loop over a list `list` knows, each field read of
/// a loop's item must be on every item `list` knows of that the task renders for (a
/// `when=` that may skip it only warns), and each `${name}` must be defined:
/// `is_defined` (for every host), a name a task before this one registers, the binding of a
/// `${for}` around it, or a host variable — `hosts` tells which hosts define it. Each name
/// is reported once per file, at its first line.
pub fn template_reference_findings(
    plan: &Plan,
    plan_dir: &Path,
    is_defined: impl Fn(&str) -> bool,
    hosts: impl Fn(&str) -> Coverage,
    list: impl Fn(&str) -> Option<List>,
) -> TemplateFindings {
    let mut registered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut findings = TemplateFindings::default();
    let presence = |var: &str, registered: &std::collections::HashSet<&str>| {
        if registered.contains(var) {
            // A skipped task leaves its `register=` undefined.
            return None;
        }
        (is_defined(var) || hosts(var) == Coverage::All).then_some(true)
    };
    let scalar = |var: &str, registered: &std::collections::HashSet<&str>| {
        registered.contains(var) || is_defined(var) || hosts(var) != Coverage::None
    };
    for step in plan.steps() {
        let looped = match &step.loop_source {
            Some(LoopSource::Variable(name)) => list(name).map(|items| (name.as_str(), items)),
            _ => None,
        };
        // The executor decides the step's `when=` once, before any of its tasks registers.
        let step_gate = GuardScope::new(
            step.when.iter(),
            &|var| presence(var, &registered),
            &list,
            &|var| scalar(var, &registered),
        )
        .reach(step.when.as_ref(), None);
        // `rescue` and `always` run after the loop; `template_scope_problems` reports it.
        let blocks = [
            (&step.tasks, Part::Tasks),
            (&step.rescue, Part::Rescue),
            (&step.always, Part::Always),
        ];
        for (task, part) in blocks
            .into_iter()
            .flat_map(|(tasks, part)| tasks.iter().map(move |task| (task, part)))
        {
            let own = part == Part::Tasks;
            if matches!(task.args.get("template"), Some(ParamValue::Bool(true)))
                && let Some((src, resolved)) = local_source(task, step.base_dir(plan_dir))
            {
                let at = |shown: &str, line: usize| {
                    format!(
                        "step '{}': file '{}': template {}, line {}",
                        step.name, task.resource, shown, line
                    )
                };
                let defined = |name: &str| is_defined(name) || registered.contains(name);
                let present = |var: &str| -> Option<bool> {
                    if is_item_var(var) {
                        return match (own, &step.loop_source, &looped) {
                            (false, ..) | (_, None, _) => Some(false),
                            (_, _, None) => Some(var == "@item"),
                            // A known item's fields are its own; see `GuardScope::reach`.
                            (_, _, Some((_, List::Items(_)))) => Some(false),
                            (_, _, Some((_, List::Unknown | List::Unresolved))) => {
                                (var == "@item").then_some(false)
                            }
                        };
                    }
                    if is_error_var(var) {
                        if !["@error.msg", "@error.task"].contains(&var) {
                            return Some(false);
                        }
                        return match part {
                            Part::Tasks => Some(false),
                            Part::Rescue => Some(true),
                            // `always` runs after a failure or none.
                            Part::Always => None,
                        };
                    }
                    presence(var, &registered)
                };
                let guards = GuardScope::new(task.when.iter(), &present, &list, &|var| {
                    scalar(var, &registered)
                });
                let gate = step_gate;
                let looped = looped.as_ref().filter(|_| own).map(|(name, items)| {
                    Looped::new(name, items, gate, task.when.as_ref(), &guards)
                });
                let item = match (&step.loop_source, &looped) {
                    _ if !own => Item::Elsewhere,
                    (None, _) => Item::NoLoop,
                    (_, Some(looped)) => Item::Fields(looped),
                    (Some(_), None) => Item::Value,
                };
                let reach = match (&looped, &step.loop_source) {
                    (Some(looped), _) => gate.and(looped.reach),
                    // Literal items are known: the task renders if its guard lets one
                    // through, and a loop over none runs no task of the step's own.
                    (None, Some(LoopSource::Literal(items))) if own => {
                        gate.and(items.iter().fold(Reach::Never, |reach, item| {
                            reach.or(guards.reach(task.when.as_ref(), Some(Bound::Value(item))))
                        }))
                    }
                    (None, _) => gate.and(guards.reach(task.when.as_ref(), None)),
                };
                let (files, unreadable) = uploaded_files(task, &resolved, &plan.vars);
                if let Some(unreadable) = unreadable {
                    findings.problems.push(format!(
                        "step '{}': file '{}': template {}: cannot be read: {}",
                        step.name, task.resource, src, unreadable
                    ));
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
                        &list,
                        Render { item, reach },
                        &mut findings,
                    );
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
#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    /// The step has no `loop=`.
    NoLoop,
    /// A loop over lines or literal items: `${@item}`.
    Value,
    /// A loop over a list variable: `${@item.<field>}`.
    Fields(&'a Looped<'a>),
    /// A `rescue` or `always` task, checked by [`template_scope_problems`].
    Elsewhere,
}

/// Where in its step a task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Tasks,
    Rescue,
    Always,
}

/// What a task's template reads `${@item…}` as, and whether the task renders it at all.
#[derive(Debug, Clone, Copy)]
struct Render<'a> {
    item: Item<'a>,
    reach: Reach,
}

/// Why `name`, an `@item` reference, is not what `item` provides, or `None` when it is.
fn item_problem(name: &str, item: Item) -> Option<&'static str> {
    let fields = name.starts_with("@item.");
    match item {
        Item::Elsewhere => None,
        Item::NoLoop => Some("the step has no `loop=`"),
        Item::Value if fields => Some("the step loops over lines or items, read as ${@item}"),
        Item::Fields(..) if !fields => {
            Some("the step loops over a list, whose fields are read as ${@item.<field>}")
        }
        Item::Value | Item::Fields(..) => None,
    }
}

fn check_template(
    text: &str,
    at: &dyn Fn(usize) -> String,
    defined: &dyn Fn(&str) -> bool,
    hosts: &dyn Fn(&str) -> Coverage,
    list: &dyn Fn(&str) -> Option<List>,
    Render { item, reach }: Render,
    findings: &mut TemplateFindings,
) {
    if let Some((line, message)) = structure_error(text) {
        findings.problems.push(format!("{}: {}", at(line), message));
        return;
    }
    let mut seen_names = std::collections::HashSet::new();
    let mut seen_fields = std::collections::HashSet::new();
    let mut seen_lists = std::collections::HashSet::new();
    let mut seen_escapes = std::collections::HashSet::new();
    let mut bindings: Vec<(&str, &str, Option<List>)> = Vec::new();
    for (line, token) in tokens(text) {
        match token {
            Token::For {
                binding,
                collection,
                ..
            } => {
                let items = list(collection);
                if items.is_none() && seen_lists.insert(collection) {
                    findings.problems.push(format!(
                        "{}: loops over '{}', which is not a defined list",
                        at(line),
                        collection
                    ));
                }
                bindings.push((binding, collection, items));
            }
            Token::EndFor => {
                bindings.pop();
            }
            Token::Var(name) => {
                // Nothing renders in a task that never runs, or inside a loop over an empty
                // list, so no field is read there.
                let unreachable = reach == Reach::Never
                    || bindings.iter().any(
                        |(.., items)| matches!(items, Some(List::Items(items)) if items.is_empty()),
                    );
                // The innermost binding wins, as in `render`.
                let bound = bindings
                    .iter()
                    .rev()
                    .find_map(|(binding, collection, items)| {
                        bound_field(name, binding).map(|field| (field, *collection, items))
                    });
                if let Some((field, collection, items)) = bound {
                    let why = items
                        .as_ref()
                        .filter(|_| !unreachable)
                        .and_then(|items| items.missing(collection, field));
                    if let Some(why) = why.filter(|_| seen_fields.insert((name, collection))) {
                        let message = format!("{}: ${{{}}}: {}", at(line), name, why);
                        if reach == Reach::Maybe {
                            findings
                                .warnings
                                .push(format!("{message}, unless a `when=` skips the task"));
                        } else {
                            findings.problems.push(message);
                        }
                    }
                    continue;
                }
                if unreachable && name.starts_with("@item.") {
                    continue;
                }
                if !seen_names.insert(name) {
                    continue;
                }
                if is_item_var(name) {
                    if let Some(why) = item_problem(name, item) {
                        findings.problems.push(format!(
                            "{}: ${{{}}} is not defined here: {}",
                            at(line),
                            name,
                            why
                        ));
                    } else if let (Some(field), Item::Fields(looped)) =
                        (name.strip_prefix("@item."), item)
                    {
                        if let Some(why) = looped.runs.missing(looped.name, field) {
                            findings
                                .problems
                                .push(format!("{}: ${{{}}}: {}", at(line), name, why));
                        } else if let Some(why) = looped.all().missing(looped.name, field) {
                            findings.warnings.push(format!(
                                "{}: ${{{}}}: {}, unless the task's `when=` skips those items",
                                at(line),
                                name,
                                why
                            ));
                        }
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
                    // No inventory sets an `@` name: only glidesh's are defined.
                    Coverage::None if name.starts_with('@') => findings.problems.push(format!(
                        "{}: ${{{}}} is not defined",
                        at(line),
                        name
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
                let bound = bindings
                    .iter()
                    .any(|(binding, ..)| bound_field(name, binding).is_some());
                if (bound || defined(name) || hosts(name) != Coverage::None)
                    && seen_escapes.insert(name)
                {
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
            if let Token::Escaped(name) = token
                && defined(name)
            {
                warnings.push(old_escape_warning(at, name));
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
        literal_reference_warnings(p, dir, |n| defined.contains(&n), defined)
    }

    #[test]
    fn an_untemplated_file_with_a_mistyped_reference_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.env"),
            "DIR=${some-var}\nHOST=${@host.nme}\nHOME=${HOME}\nX=${elsewhere}\n",
        )
        .unwrap();
        let p = plan(r#"step "Env" { file "/etc/app.env" src="app.env" }"#);
        let w = warnings_for(&p, dir.path(), &["some-vars", "@host.name"]);
        assert_eq!(
            w,
            [
                "step 'Env': app.env contains undefined ${@host.nme} (did you mean \
              ${@host.name}?), ${some-var} (did you mean ${some-vars}?); it is uploaded \
              as-is, because `template` is not set, so it ships literally"
            ]
        );
    }

    /// A defined name, a misspelled glidesh-style name, a mistyped built-in and a loop each
    /// warn; a shell variable and the loop's own field do not.
    #[test]
    fn an_untemplated_file_with_template_syntax_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plain.conf"),
            "a ${known}
b ${knwon-typo}
c ${@host.nme}
             d ${for x in @group.g}${x.name}${endfor}
e ${HOME}
",
        )
        .unwrap();
        let p = plan(r#"step "plain" { file "/tmp/plain.conf" src="plain.conf" }"#);
        let w = warnings_for(&p, dir.path(), &["known", "@host.name"]);
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("contains a ${for} loop but"), "{}", w[0]);
        assert!(w[1].contains("contains ${known} but"), "{}", w[1]);
        assert!(
            w[2].contains(
                "contains undefined ${@host.nme} (did you mean ${@host.name}?), ${knwon-typo};"
            ),
            "{}",
            w[2]
        );
        assert!(!w.concat().contains("x.name"), "{w:?}");
        assert!(!w.concat().contains("HOME"), "{w:?}");
    }

    #[test]
    fn a_shell_variable_close_to_a_defined_name_does_not_warn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("run.sh"),
            "cd ${HOME}
echo ${VAR-default}
",
        )
        .unwrap();
        let p = plan(r#"step "s" { file "/run.sh" src="run.sh" }"#);
        assert!(warnings_for(&p, dir.path(), &["HOMES", "VAR-defaults"]).is_empty());
    }

    #[test]
    fn a_loop_field_read_outside_its_loop_still_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plain.conf"),
            "${for x in xs}${x.name}${endfor}${x.naem}
",
        )
        .unwrap();
        let p = plan(r#"step "plain" { file "/tmp/plain.conf" src="plain.conf" }"#);
        let w = warnings_for(&p, dir.path(), &[]);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[1].contains("contains undefined ${x.naem};"), "{}", w[1]);
        assert!(!w[1].contains("x.name}"), "{}", w[1]);
    }

    #[test]
    fn only_names_no_shell_writes_are_glidesh_style() {
        for name in ["knwon-typo", "app.dir", "@host.nme", "db_host-2"] {
            assert!(glidesh_style(name), "{name}");
        }
        for name in [
            "HOME",
            "home",
            "VAR-default",
            "HOME:-/root",
            "var-",
            "@",
            "1-x",
            "x:-y",
        ] {
            assert!(!glidesh_style(name), "{name}");
        }
    }

    #[test]
    fn a_registered_name_in_an_untemplated_file_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.env"), "V=${version}\n").unwrap();
        let p = plan(
            r#"step "Env" {
                shell "cat VERSION" register="version"
                file "/etc/app.env" src="app.env"
            }"#,
        );
        let w = warnings_for(&p, dir.path(), &[]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("contains ${version} but"), "{}", w[0]);
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
            |name| {
                ["port", "db-host"].contains(&name)
                    || name.starts_with("@host.")
                    || name.starts_with("@error.")
            },
            |name| match name {
                "region" => Coverage::All,
                "tier" => Coverage::Partial(vec!["db-1".to_string(), "db-2".to_string()]),
                "flags" => Coverage::Partial(vec!["db-1".to_string()]),
                _ => Coverage::None,
            },
            list_of,
        )
    }

    /// `@group.db`, `vms`, `flags` and `empty` have known items; `vms`' second, `db`, has no
    /// `port`, and `flags`' first has an encrypted `enabled`.
    fn list_of(name: &str) -> Option<List> {
        let items = |items: &[&[&str]]| {
            List::Items(
                items
                    .iter()
                    .map(|fields| {
                        fields
                            .iter()
                            .map(|f| {
                                let (field, value) = f.split_once('=').unwrap_or((f, ""));
                                (field.to_string(), value.to_string())
                            })
                            .collect()
                    })
                    .collect(),
            )
        };
        match name {
            "api-keys" | "hosts" | "@group.web" => Some(List::Unknown),
            "@group.optional" => Some(List::Unresolved),
            "@group.db" => {
                let host: &[&str] = &["name", "address", "user", "port"];
                Some(items(&[host, host]))
            }
            "vms" => Some(items(&[&["name=web", "port=80"], &["name=db"]])),
            "flags" => Some(items(&[
                &["name=web", "enabled=secret:v1:AAAA"],
                &["name=db", "enabled=no"],
            ])),
            "empty" => Some(items(&[])),
            _ => None,
        }
    }

    #[test]
    fn a_field_no_item_has_is_a_problem() {
        let found = template_findings(
            "${for a in @group.db}\n${a.address} ${a.bogus} ${a.bogus}\n${endfor}",
            TEMPLATED,
        );
        assert_eq!(
            found.problems,
            [
                "step 'Deploy': file '/etc/app.conf': template app.conf, line 2: ${a.bogus}: no \
              item of '@group.db' has a field 'bogus' (available: address, name, port, user)"
            ]
        );
    }

    #[test]
    fn a_field_only_some_items_have_is_a_problem() {
        let found = template_findings("${for v in vms}${v.port}${endfor}", TEMPLATED);
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("${v.port}: 1 of the 2 items of 'vms' have no field 'port'"),
            "{}",
            found.problems[0]
        );
    }

    /// Unknown items may have any field; an empty list renders no item at all.
    #[test]
    fn fields_of_unknown_or_empty_lists_are_not_checked() {
        let found = template_findings(
            "${for k in api-keys}${k.bogus}${endfor}${for e in empty}${e.bogus}${endfor}",
            TEMPLATED,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
    }

    #[test]
    fn a_field_is_read_from_the_innermost_binding() {
        let found = template_findings(
            "${for a in vms}${for a in @group.db}${a.address}${endfor}${a.address}${endfor}",
            TEMPLATED,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("no item of 'vms' has a field 'address'"),
            "{}",
            found.problems[0]
        );
    }

    #[test]
    fn a_loop_items_field_is_checked_against_the_list() {
        let found = template_findings(
            "${@item.name} ${@item.bogus}",
            r#"step "Deploy" loop="${vms}" {
                file "/etc/app.conf" src="app.conf" template=#true
            }"#,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains(
                "${@item.bogus}: no item of 'vms' has a field 'bogus' (available: name, port)"
            ),
            "{}",
            found.problems[0]
        );
    }

    #[test]
    fn a_field_inside_a_loop_over_an_empty_list_is_never_read() {
        let found = template_findings(
            "${for e in empty}${for v in vms}${v.port}${endfor}${@item.port}${endfor}             ${for v in vms}${v.name}${endfor}${@item.bogus}",
            r#"step "Deploy" loop="${vms}" {
                file "/etc/app.conf" src="app.conf" template=#true
            }"#,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("${@item.bogus}: no item of 'vms'"),
            "{}",
            found.problems[0]
        );
    }

    fn guarded(template: &str, when: &str) -> TemplateFindings {
        template_findings(
            template,
            &format!(
                r#"step "Deploy" loop="${{vms}}" {{
                    file "/etc/app.conf" src="app.conf" template=#true when="{when}"
                }}"#
            ),
        )
    }

    /// The executor evaluates the guard per item before rendering, so an item it skips
    /// never reaches the template.
    #[test]
    fn a_when_guard_leaves_out_the_items_it_skips() {
        for when in [
            "defined ${@item.port}",
            "${@item.name} == web",
            "defined ${env} && defined ${@item.port}",
        ] {
            let found = guarded("${@item.port}", when);
            assert!(found.problems.is_empty(), "{when}: {:?}", found.problems);
            assert!(found.warnings.is_empty(), "{when}: {:?}", found.warnings);
        }
    }

    /// An item a guard lets through is checked, and so is one it fails on: reading `port`
    /// in the guard fails the item without it, so the run does.
    #[test]
    fn a_guard_true_for_an_item_still_checks_its_fields() {
        for when in [
            "defined ${@item.name}",
            "defined ${vms}",
            "${@item.port} == 80",
        ] {
            let found = guarded("${@item.port}", when);
            assert_eq!(found.problems.len(), 1, "{when}: {:?}", found.problems);
            assert!(
                found.problems[0].contains("1 of the 2 items of 'vms' have no field 'port'"),
                "{when}: {}",
                found.problems[0]
            );
        }
        let found = guarded("${@item.bogus}", "${@item.name} == web");
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(found.problems[0].contains("no item of 'vms' has a field 'bogus'"));
    }

    /// A guard on a value only a host knows may or may not skip the item: a warning, not a
    /// problem, and never silence.
    #[test]
    fn an_undecided_guard_warns() {
        let found = guarded("${@item.port}", "${env} == prod");
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0].contains(
                "1 of the 2 items of 'vms' have no field 'port', unless the task's `when=` \
                 skips those items"
            ),
            "{}",
            found.warnings[0]
        );
        let found = guarded("${for v in vms}${v.port}${endfor}", "${env} == prod");
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(
            found.warnings[0].contains("unless a `when=` skips the task"),
            "{}",
            found.warnings[0]
        );
    }

    /// A task whose `register=` is skipped leaves its name undefined, so a guard on it is
    /// undecided, never false.
    #[test]
    fn a_guard_on_a_registered_name_is_undecided() {
        let found = template_findings(
            "${@item.bogus}",
            r#"step "Probe" { shell "echo x" register="out" when="false" }
            step "Deploy" loop="${vms}" {
                file "/etc/app.conf" src="app.conf" template=#true when="undefined ${out}"
            }"#,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0].contains("no item of 'vms' has a field 'bogus'"),
            "{}",
            found.warnings[0]
        );
    }

    /// The host compares the decrypted value, not the token.
    #[test]
    fn a_guard_on_an_encrypted_value_is_undecided() {
        let found = template_findings(
            "${@item.port}",
            r#"step "Deploy" loop="${flags}" {
                file "/etc/app.conf" src="app.conf" template=#true when="${@item.enabled} == yes"
            }"#,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0].contains("no item of 'flags' has a field 'port'"),
            "{}",
            found.warnings[0]
        );
    }

    /// A task or step guard skips the render whether or not the step loops.
    #[test]
    fn a_guard_without_a_loop_decides_whether_the_template_renders() {
        let template = "${for v in vms}${v.port}${endfor}";
        let skipped = [
            r#"step "Deploy" { file "/etc/app.conf" src="app.conf" template=#true when="false" }"#,
            r#"step "Deploy" when="false" { file "/etc/app.conf" src="app.conf" template=#true }"#,
        ];
        for body in skipped {
            let found = template_findings(template, body);
            assert!(found.problems.is_empty(), "{body}: {:?}", found.problems);
            assert!(found.warnings.is_empty(), "{body}: {:?}", found.warnings);
        }
        let found = template_findings(
            template,
            r#"step "Deploy" {
                file "/etc/app.conf" src="app.conf" template=#true when="${env} == prod"
            }"#,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(
            found.warnings[0].contains("have no field 'port', unless a `when=` skips the task"),
            "{}",
            found.warnings[0]
        );
    }

    fn in_step(attrs: &str, task_when: &str) -> String {
        format!(
            r#"step "Deploy" {attrs} {{
                file "/etc/app.conf" src="app.conf" template=#true when="{task_when}"
            }}"#
        )
    }

    const VMS_PORT: &str = "${for v in vms}${v.port}${endfor}";

    /// A loop over lines binds `${@item}` alone, so no `${@item.<field>}` is defined.
    #[test]
    fn a_line_loop_has_no_item_fields_for_a_guard() {
        let found = template_findings(
            VMS_PORT,
            &in_step(r#"loop="${lines}""#, "defined ${@item.port}"),
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    /// `rescue` runs only after a failure, `always` after one or none.
    #[test]
    fn error_variables_are_defined_in_rescue_and_maybe_in_always() {
        let body = |block: &str| {
            format!(
                r#"step "Deploy" {{
                    shell "true"
                    {block} {{
                        file "/etc/app.conf" src="app.conf" template=#true when="undefined ${{@error.msg}}"
                    }}
                }}"#
            )
        };
        let found = template_findings(VMS_PORT, &body("rescue"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        let found = template_findings(VMS_PORT, &body("always"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0].contains("unless a `when=` skips the task"),
            "{}",
            found.warnings[0]
        );
    }

    /// Items `validate` cannot see may have any field, but the task's guard still decides
    /// whether it renders.
    #[test]
    fn a_guard_over_an_unknown_list_decides_whether_the_template_renders() {
        let loop_attr = r#"loop="${api-keys}""#;
        let found = template_findings(VMS_PORT, &in_step(loop_attr, "false"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        for when in ["${env} == prod", "defined ${@item.port}"] {
            let found = template_findings(VMS_PORT, &in_step(loop_attr, when));
            assert!(found.problems.is_empty(), "{when}: {:?}", found.problems);
            assert_eq!(found.warnings.len(), 1, "{when}: {:?}", found.warnings);
        }
    }

    /// `region` is set on every host the plan runs on.
    #[test]
    fn a_variable_every_host_sets_is_defined_for_a_guard() {
        let found = template_findings(
            "${@item.port}",
            &in_step(r#"loop="${vms}""#, "defined ${region}"),
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        let found = template_findings(
            "${@item.port}",
            &in_step(r#"loop="${vms}""#, "undefined ${region}"),
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    /// Without an inventory, a group may not exist at all.
    #[test]
    fn a_guard_on_an_unresolved_group_is_undecided() {
        let found = template_findings(
            "${@item.port}",
            &in_step(r#"loop="${vms}""#, "undefined ${@group.optional}"),
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    }

    /// The step's `when=` is decided before its tasks run, so their `register=` cannot change
    /// it.
    #[test]
    fn a_step_guard_sees_what_was_registered_before_the_step() {
        let found = template_findings(
            "${@item.port}",
            r#"step "Deploy" loop="${vms}" when="defined ${port}" {
                shell "echo 1" register="port"
                file "/etc/app.conf" src="app.conf" template=#true
            }"#,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    /// A guard compares the value of a list's name when a host or a task sets one, which
    /// only a run knows.
    #[test]
    fn a_value_sharing_a_lists_name_leaves_a_guard_undecided() {
        let found = template_findings(VMS_PORT, &in_step("", "${empty} == on"));
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        let found = template_findings(VMS_PORT, &in_step("", "${flags} == on"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        let found = template_findings(
            VMS_PORT,
            r#"step "Deploy" {
                shell "echo on" register="empty"
                file "/etc/app.conf" src="app.conf" template=#true when="${empty} == on"
            }"#,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    }

    /// A literal loop's items are known, so a guard on `${@item}` is decided per item.
    #[test]
    fn a_guard_on_a_literal_item_is_decided_by_its_value() {
        let found = template_findings(VMS_PORT, &in_step(r#"loop="web""#, "${@item} == web"));
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        let found = template_findings(VMS_PORT, &in_step(r#"loop="db""#, "${@item} == web"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        let found = template_findings(VMS_PORT, &in_step(r#"loop="db\nweb""#, "${@item} == web"));
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
    }

    /// Only `@error.msg` and `@error.task` are ever bound.
    #[test]
    fn an_error_variable_glidesh_never_sets_is_undefined_in_rescue() {
        let found = template_findings(
            VMS_PORT,
            r#"step "Deploy" {
                shell "true"
                rescue {
                    file "/etc/app.conf" src="app.conf" template=#true when="undefined ${@error.code}"
                }
            }"#,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
    }

    #[test]
    fn a_literal_loop_over_no_items_renders_nothing() {
        let found = template_findings(VMS_PORT, &in_step(r#"loop="""#, "true"));
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        let found = template_findings(
            VMS_PORT,
            r#"step "Deploy" loop="" {
                shell "true"
                always { file "/etc/app.conf" src="app.conf" template=#true }
            }"#,
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
    }

    #[test]
    fn a_task_that_renders_for_no_item_is_not_field_checked() {
        let template = "${for v in vms}${v.port}${endfor}${@item.bogus}";
        let found = guarded(template, "defined ${@item.bogus}");
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        let found = template_findings(
            template,
            r#"step "Deploy" loop="${empty}" {
                file "/etc/app.conf" src="app.conf" template=#true
            }"#,
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
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
        let found = template_reference_findings(
            &p,
            dir.path(),
            |_| true,
            |_| Coverage::All,
            |_| Some(List::Unknown),
        );
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("is not valid UTF-8"),
            "{}",
            found.problems[0]
        );
    }

    #[test]
    fn an_unknown_reserved_name_is_a_problem_even_without_an_inventory() {
        let found = template_findings("${@os.idd} ${@fact.unknown}\n", TEMPLATED);
        assert_eq!(found.problems.len(), 2, "{:?}", found.problems);
        assert!(found.problems[0].contains("${@os.idd} is not defined"));
        assert!(found.undefined.is_empty(), "{:?}", found.undefined);
    }

    #[test]
    fn an_escaped_loop_field_warns_it_changed_meaning() {
        let found = template_findings("${for h in hosts}$${h.name}${endfor}", TEMPLATED);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(found.warnings[0].contains("$${h.name} writes a literal ${h.name}"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_of_a_recursive_template_is_a_problem() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::process::Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&root.stdout).trim() == "0" {
            return; // root reads any directory
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("conf").join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let p = plan(r#"step "s" { file "/etc/app/" src="conf" recurse=#true template=#true }"#);
        let found = template_reference_findings(
            &p,
            dir.path(),
            |_| true,
            |_| Coverage::All,
            |_| Some(List::Unknown),
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("cannot be read"),
            "{}",
            found.problems[0]
        );
    }

    #[test]
    fn misused_recursive_parameters_are_problems() {
        let p = plan(
            r#"step "s" {
                file "/etc/a" src="a" dir-mode="0750"
                file "/srv" src="site" recurse=#true prune=#true
                file "/srv/app" src="site" recurse=#true exclude="../x"
                file "/srv/${app}" src="site" recurse=#true prune=#true
                file "/srv/app" src="site" recurse=#true prune=#true exclude=".git"
            }"#,
        );
        let problems = file_option_problems(&p);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("file '/etc/a': dir-mode= only apply with recurse=#true"));
        assert!(problems[1].contains("file '/srv': prune=#true needs an absolute destination"));
        assert!(problems[2].contains("may not contain empty, `.` or `..` parts"));
    }

    /// The plan's vars are known before any host: a value they give is checked as the run
    /// will see it.
    #[test]
    fn recursive_options_are_checked_with_the_plans_vars_filled_in() {
        let p = plan(
            r#"vars {
                root "/srv"
                bad "../x"
                m "u=rw"
            }
            step "s" {
                file "${root}" src="site" recurse=#true prune=#true
                file "/srv/app" src="site" recurse=#true exclude="${bad}"
                file "/etc/a" src="a" mode="${m}"
                file "/srv/b" src="site" recurse=#true { exclude { - "../y"; - "${@host.name}"; } }
                file "${@host.name}" src="site" recurse=#true prune=#true exclude="${@host.name}" mode="${@host.name}"
            }"#,
        );
        let problems = file_option_problems(&p);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("prune=#true needs an absolute destination"));
        assert!(problems[1].contains("may not contain empty, `.` or `..` parts"));
        assert!(problems[2].contains("mode=\"u=rw\" must be an octal mode"));
        assert!(
            problems[3].contains("\"../y\""),
            "a host-only pattern beside it does not hide it: {}",
            problems[3]
        );
    }

    /// An excluded file is never uploaded, so a template check skips it: a binary `.git`
    /// object is no template.
    #[test]
    fn a_template_check_skips_what_exclude_leaves_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conf/.git")).unwrap();
        std::fs::write(dir.path().join("conf/app.conf"), "port=${port}").unwrap();
        std::fs::write(dir.path().join("conf/.git/index"), b"\xff${nope}").unwrap();
        let p = plan(
            r#"step "s" {
                file "/etc/app/" src="conf" recurse=#true template=#true {
                    exclude {
                        - ".git"
                    }
                }
            }"#,
        );
        let found = template_reference_findings(
            &p,
            dir.path(),
            |n| n == "port",
            |_| Coverage::All,
            |_| Some(List::Unknown),
        );
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert!(found.undefined.is_empty(), "{:?}", found.undefined);
    }

    /// A run interpolates `exclude` first: a pattern the plan's vars resolve leaves out what
    /// the run does; one only a host resolves leaves the tree unchecked, not misread.
    #[test]
    fn an_interpolated_exclude_is_read_as_a_run_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conf/.git")).unwrap();
        std::fs::write(dir.path().join("conf/app.conf"), "port=${port}").unwrap();
        std::fs::write(dir.path().join("conf/.git/index"), b"\xff${nope}").unwrap();
        for (vars, pattern) in [
            (r#"vars { ignored ".git" }"#, "${ignored}"),
            ("", "${@host.name}"),
        ] {
            let p = plan(&format!(
                r#"{vars}
                step "s" {{
                    file "/etc/app/" src="conf" recurse=#true template=#true {{
                        exclude {{
                            - "{pattern}"
                        }}
                    }}
                }}"#
            ));
            let found = template_reference_findings(
                &p,
                dir.path(),
                |n| n == "port",
                |_| Coverage::All,
                |_| Some(List::Unknown),
            );
            assert!(found.problems.is_empty(), "{pattern}: {:?}", found.problems);
            assert!(
                found.undefined.is_empty(),
                "{pattern}: {:?}",
                found.undefined
            );
        }
    }
}
