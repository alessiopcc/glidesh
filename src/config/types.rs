use crate::config::condition::Condition;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct JumpHost {
    pub address: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// A `jump` node: through a bastion, or `jump #false` to reach the host directly although a
/// wider scope names one.
#[derive(Debug, Clone)]
pub enum Jump {
    Via(JumpHost),
    Direct,
}

#[derive(Debug, Clone)]
pub struct ResolvedJumpHost {
    pub address: String,
    pub user: String,
    pub port: u16,
}

/// Privilege escalation method used to run a command as another user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAsMethod {
    #[default]
    Sudo,
    Doas,
    Su,
}

impl RunAsMethod {
    pub fn parse(s: &str) -> Option<RunAsMethod> {
        match s {
            "sudo" => Some(RunAsMethod::Sudo),
            "doas" => Some(RunAsMethod::Doas),
            "su" => Some(RunAsMethod::Su),
            _ => None,
        }
    }
}

/// The escalation target at a single config level.
///
/// `run-as="x"` => `User("x")`, `run-as=""` => `Disabled` (cancel an escalated
/// parent), attribute absent => the surrounding `RunAsSpec.user` is `None` (inherit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAsUser {
    Disabled,
    User(String),
}

/// Partial run-as config at one level (host/group/global/step/task/CLI). Merged
/// field-by-field down the chain, most specific winning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunAsSpec {
    /// `None` = inherit from the less-specific level.
    pub user: Option<RunAsUser>,
    /// `None` = inherit; the final fallback is [`RunAsMethod::Sudo`].
    pub method: Option<RunAsMethod>,
}

impl RunAsSpec {
    /// Layer `self` (more specific) over `base` (less specific). Each field falls
    /// back to `base` only when unset on `self`.
    pub fn merge_over(self, base: &RunAsSpec) -> RunAsSpec {
        RunAsSpec {
            user: self.user.or_else(|| base.user.clone()),
            method: self.method.or(base.method),
        }
    }

    /// Resolve to a concrete escalation, attaching the global password. Returns
    /// `None` when escalation is unset or explicitly disabled.
    pub fn resolve(&self, password: Option<&str>) -> Option<ResolvedRunAs> {
        match &self.user {
            Some(RunAsUser::User(user)) => Some(ResolvedRunAs {
                user: user.clone(),
                method: self.method.unwrap_or_default(),
                password: password.map(|p| p.to_string()),
            }),
            _ => None,
        }
    }
}

/// A fully resolved escalation, ready to wrap a command.
#[derive(Debug, Clone)]
pub struct ResolvedRunAs {
    pub user: String,
    pub method: RunAsMethod,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Host {
    pub name: String,
    pub address: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub vars: HashMap<String, String>,
    pub plan: Option<String>,
    pub jump: Option<Jump>,
    pub run_as: RunAsSpec,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub hosts: Vec<Host>,
    pub vars: HashMap<String, String>,
    pub plan: Option<String>,
    pub jump: Option<Jump>,
    pub run_as: RunAsSpec,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub groups: Vec<Group>,
    pub ungrouped_hosts: Vec<Host>,
    pub global_vars: HashMap<String, String>,
    pub run_as: RunAsSpec,
    /// The top-level `jump`: every host's bastion unless its group or itself names another.
    pub jump: Option<JumpHost>,
}

impl Inventory {
    /// Resolve hosts matching a target filter.
    /// Accepted forms: `None` (all hosts), `"name"` (group or host name),
    /// `"group:host"` (specific host within a specific group), or a
    /// comma-separated list combining any of the above.
    pub fn resolve_targets(&self, target: Option<&str>) -> Vec<ResolvedHost> {
        if let Some(t) = target {
            if t.contains(',') {
                let mut out: Vec<ResolvedHost> = Vec::new();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for piece in t.split(',') {
                    let piece = piece.trim();
                    if piece.is_empty() {
                        continue;
                    }
                    for h in self.resolve_targets(Some(piece)) {
                        if seen.insert(h.name.clone()) {
                            out.push(h);
                        }
                    }
                }
                return out;
            }
        }

        let mut hosts = Vec::new();

        match target {
            None => {
                for group in &self.groups {
                    for host in &group.hosts {
                        hosts.push(self.resolve_host(host, Some(group)));
                    }
                }
                for host in &self.ungrouped_hosts {
                    hosts.push(self.resolve_host(host, None));
                }
            }
            Some(target) => {
                if let Some((g_name, h_name)) = target.split_once(':') {
                    for group in &self.groups {
                        if group.name == g_name {
                            for host in &group.hosts {
                                if host.name == h_name {
                                    hosts.push(self.resolve_host(host, Some(group)));
                                    return hosts;
                                }
                            }
                        }
                    }
                    return hosts;
                }
                // Try group match first
                for group in &self.groups {
                    if group.name == target {
                        for host in &group.hosts {
                            hosts.push(self.resolve_host(host, Some(group)));
                        }
                        return hosts;
                    }
                }
                // Try host match
                for group in &self.groups {
                    for host in &group.hosts {
                        if host.name == target {
                            hosts.push(self.resolve_host(host, Some(group)));
                            return hosts;
                        }
                    }
                }
                for host in &self.ungrouped_hosts {
                    if host.name == target {
                        hosts.push(self.resolve_host(host, None));
                        return hosts;
                    }
                }
            }
        }

        hosts
    }

    /// Returns groups/hosts that have an associated plan path, with their resolved hosts.
    /// Used when running without a CLI `--plan` flag.
    /// Each entry is (label, plan_path, resolved_hosts).
    pub fn resolve_group_plans(&self) -> Vec<(String, String, Vec<ResolvedHost>)> {
        let mut result = Vec::new();
        for group in &self.groups {
            if let Some(ref plan_path) = group.plan {
                // Group-plan entry: hosts that inherit the group plan
                // (those without their own plan= attribute).
                let hosts: Vec<ResolvedHost> = group
                    .hosts
                    .iter()
                    .filter(|h| h.plan.is_none())
                    .map(|h| self.resolve_host(h, Some(group)))
                    .collect();
                if !hosts.is_empty() {
                    result.push((group.name.clone(), plan_path.clone(), hosts));
                }
            }
            // Hosts inside a group that override with their own plan attribute.
            for host in &group.hosts {
                if let Some(ref plan_path) = host.plan {
                    let resolved = self.resolve_host(host, Some(group));
                    result.push((group.name.clone(), plan_path.clone(), vec![resolved]));
                }
            }
        }
        for host in &self.ungrouped_hosts {
            if let Some(ref plan_path) = host.plan {
                let resolved = self.resolve_host(host, None);
                result.push((String::new(), plan_path.clone(), vec![resolved]));
            }
        }
        result
    }

    fn resolve_host(&self, host: &Host, group: Option<&Group>) -> ResolvedHost {
        // Merge vars: global -> group -> host (most specific wins)
        let mut vars = self.global_vars.clone();
        if let Some(g) = group {
            vars.extend(g.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        vars.extend(host.vars.iter().map(|(k, v)| (k.clone(), v.clone())));

        let user = host
            .user
            .clone()
            .or_else(|| vars.get("deploy-user").cloned())
            .unwrap_or_else(|| "root".to_string());

        // The most specific `jump` decides, `jump #false` included.
        let jump_source = match host.jump.as_ref().or_else(|| group?.jump.as_ref()) {
            Some(Jump::Via(jump)) => Some(jump),
            Some(Jump::Direct) => None,
            None => self.jump.as_ref(),
        };
        let jump = jump_source.map(|j| ResolvedJumpHost {
            address: j.address.clone(),
            user: j.user.clone().unwrap_or_else(|| user.clone()),
            port: j.port.unwrap_or(22),
        });

        // Escalation: host overrides group overrides global.
        let run_as = host
            .run_as
            .clone()
            .merge_over(group.map(|g| &g.run_as).unwrap_or(&RunAsSpec::default()))
            .merge_over(&self.run_as);

        ResolvedHost {
            name: host.name.clone(),
            address: host.address.clone(),
            user,
            port: host.port.unwrap_or(22),
            vars,
            jump,
            run_as,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedHost {
    pub name: String,
    pub address: String,
    pub user: String,
    pub port: u16,
    pub vars: HashMap<String, String>,
    pub jump: Option<ResolvedJumpHost>,
    /// Merged escalation from global -> group -> host (CLI default applied later
    /// in the executor, since it is the least-specific layer).
    pub run_as: RunAsSpec,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ExecutionMode {
    #[default]
    Sync,
    Async,
}

/// A number of hosts, given directly or as a share of the hosts a plan runs on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Amount {
    Count(usize),
    /// 0–100.
    Percent(u8),
}

/// A variable `glidesh run` asks for before connecting, declared in a plan's `vars-prompt`.
#[derive(Debug, Clone, PartialEq)]
pub struct VarPrompt {
    pub name: String,
    /// The question shown to the operator.
    pub text: String,
    /// Taken on an empty answer, and without asking when stdin is not a terminal.
    pub default: Option<String>,
    /// Read without echo and masked as `***` wherever run output shows it.
    pub secret: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    pub mode: ExecutionMode,
    /// Batch sizes for a rolling run, used in order with the last repeating. Empty runs every
    /// host in one batch.
    pub serial: Vec<Amount>,
    /// How many hosts may fail before no further batch starts. `None` stops only when a whole
    /// batch fails.
    pub max_fail: Option<Amount>,
    pub vars: HashMap<String, String>,
    /// Structured vars for template loops: each key maps to a list of named-field maps.
    pub structured_vars: HashMap<String, Vec<HashMap<String, String>>>,
    /// Paths to external KDL files containing additional vars (resolved during `resolve_includes`).
    pub vars_files: Vec<String>,
    /// Variables asked for at run time. Only the top-level plan may declare them.
    pub prompts: Vec<VarPrompt>,
    /// Plan-level escalation default, applied to every step (overridable per step/task).
    pub run_as: RunAsSpec,
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone)]
pub enum PlanItem {
    Step(Box<Step>),
    Include(Include),
}

/// An `include "path"` in a plan, before `resolve_includes` inlines it.
#[derive(Debug, Clone, PartialEq)]
pub struct Include {
    /// Path to the included plan, relative to the including plan's directory.
    pub path: String,
    /// `tags=`: added to every step the included plan brings in, nested includes too.
    pub tags: Vec<String>,
}

impl Plan {
    /// Return only the Step items (useful after resolve_includes has flattened everything).
    pub fn steps(&self) -> Vec<&Step> {
        self.items
            .iter()
            .filter_map(|item| match item {
                PlanItem::Step(s) => Some(s.as_ref()),
                PlanItem::Include(_) => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoopSource {
    Variable(String),
    Literal(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub tasks: Vec<TaskDef>,
    pub loop_source: Option<LoopSource>,
    pub subscribe: Vec<String>,
    /// Step-level escalation override (applies to all tasks in the step).
    pub run_as: RunAsSpec,
    /// Evaluated once, before the step's `loop` is resolved — so it can guard a loop over a
    /// variable that might not exist, and cannot see `@item`.
    pub when: Option<Condition>,
    /// Selects the step with `--tags` / `--skip-tags`; see `config::tags`.
    pub tags: Vec<String>,
    /// A command polled on the host before the step's tasks, until it exits 0.
    pub until: Option<UntilGate>,
    /// Run when the step fails once it has started (its `until=` gate, loop or tasks). If
    /// they succeed the failure is handled and the host goes on; they can read
    /// `${@error.msg}` and `${@error.task}`.
    pub rescue: Vec<TaskDef>,
    /// Run after the step's tasks and any `rescue`, whether or not they failed.
    pub always: Vec<TaskDef>,
    /// Directory of the included plan this step came from, set by `resolve_includes`;
    /// `None` for the top-level plan's own steps. Relative `file` sources resolve from here.
    pub source_dir: Option<std::path::PathBuf>,
}

/// A step's `until=` gate. Waiting is not work: the gate never counts as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntilGate {
    /// As written in the plan; interpolated per host when the gate runs.
    pub command: String,
    /// Seconds before the step fails.
    pub timeout: u64,
    /// Seconds between attempts.
    pub interval: u64,
}

impl UntilGate {
    pub const DEFAULT_TIMEOUT: u64 = 300;
    pub const DEFAULT_INTERVAL: u64 = 3;
    /// A week. Longer is certainly a mistake, and an unbounded value would overflow the
    /// deadline computed from it.
    pub const MAX_SECONDS: u64 = 7 * 24 * 60 * 60;
}

impl Step {
    /// The directory this step's relative paths resolve from.
    pub fn base_dir<'a>(&'a self, plan_dir: &'a std::path::Path) -> &'a std::path::Path {
        self.source_dir.as_deref().unwrap_or(plan_dir)
    }

    /// Every task the step declares: its own, then its `rescue` and `always` tasks.
    pub fn all_tasks(&self) -> impl Iterator<Item = &TaskDef> {
        self.tasks.iter().chain(&self.rescue).chain(&self.always)
    }
}

#[derive(Debug, Clone)]
pub struct TaskDef {
    pub module: String,
    pub resource: String,
    pub args: HashMap<String, ParamValue>,
    pub register: Option<String>,
    /// Module-level escalation override (most specific).
    pub run_as: RunAsSpec,
    /// Evaluated per task and per loop iteration, after `@item` is bound.
    pub when: Option<Condition>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamValue {
    String(String),
    Integer(i64),
    Bool(bool),
    List(Vec<String>),
    Map(HashMap<String, String>),
}

impl ParamValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ParamValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ParamValue::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            ParamValue::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&HashMap<String, String>> {
        match self {
            ParamValue::Map(m) => Some(m),
            _ => None,
        }
    }
}

#[cfg(test)]
mod run_as_tests {
    use super::*;

    fn user(name: &str) -> RunAsSpec {
        RunAsSpec {
            user: Some(RunAsUser::User(name.to_string())),
            method: None,
        }
    }

    #[test]
    fn merge_inherits_when_unset() {
        let base = user("root");
        let merged = RunAsSpec::default().merge_over(&base);
        assert_eq!(merged.user, Some(RunAsUser::User("root".to_string())));
    }

    #[test]
    fn merge_more_specific_wins() {
        let base = user("root");
        let specific = user("postgres");
        assert_eq!(
            specific.merge_over(&base).user,
            Some(RunAsUser::User("postgres".to_string()))
        );
    }

    #[test]
    fn disabled_cancels_escalated_parent() {
        let base = user("root");
        let off = RunAsSpec {
            user: Some(RunAsUser::Disabled),
            method: None,
        };
        let merged = off.merge_over(&base);
        assert_eq!(merged.user, Some(RunAsUser::Disabled));
        assert!(merged.resolve(None).is_none());
    }

    #[test]
    fn method_resolves_with_default_sudo() {
        let resolved = user("root").resolve(Some("pw")).unwrap();
        assert_eq!(resolved.user, "root");
        assert_eq!(resolved.method, RunAsMethod::Sudo);
        assert_eq!(resolved.password.as_deref(), Some("pw"));
    }

    #[test]
    fn full_precedence_module_over_step_over_host() {
        // host=root(sudo), step inherits, module overrides user+method.
        let host = RunAsSpec {
            user: Some(RunAsUser::User("root".to_string())),
            method: Some(RunAsMethod::Sudo),
        };
        let step = RunAsSpec::default();
        let module = RunAsSpec {
            user: Some(RunAsUser::User("deploy".to_string())),
            method: Some(RunAsMethod::Doas),
        };
        let effective = module
            .merge_over(&step)
            .merge_over(&host)
            .resolve(None)
            .unwrap();
        assert_eq!(effective.user, "deploy");
        assert_eq!(effective.method, RunAsMethod::Doas);
    }

    #[test]
    fn unset_everywhere_is_no_escalation() {
        let effective = RunAsSpec::default()
            .merge_over(&RunAsSpec::default())
            .resolve(None);
        assert!(effective.is_none());
    }
}
