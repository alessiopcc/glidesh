use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct JumpHost {
    pub address: String,
    pub user: Option<String>,
    pub port: Option<u16>,
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
    pub jump: Option<JumpHost>,
    pub run_as: RunAsSpec,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub hosts: Vec<Host>,
    pub vars: HashMap<String, String>,
    pub plan: Option<String>,
    pub jump: Option<JumpHost>,
    pub run_as: RunAsSpec,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub groups: Vec<Group>,
    pub ungrouped_hosts: Vec<Host>,
    pub global_vars: HashMap<String, String>,
    pub run_as: RunAsSpec,
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

        let jump_source = host
            .jump
            .as_ref()
            .or_else(|| group.and_then(|g| g.jump.as_ref()));
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

#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    pub mode: ExecutionMode,
    pub vars: HashMap<String, String>,
    /// Structured vars for template loops: each key maps to a list of named-field maps.
    pub structured_vars: HashMap<String, Vec<HashMap<String, String>>>,
    /// Paths to external KDL files containing additional vars (resolved during `resolve_includes`).
    pub vars_files: Vec<String>,
    /// Plan-level escalation default, applied to every step (overridable per step/task).
    pub run_as: RunAsSpec,
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone)]
pub enum PlanItem {
    Step(Step),
    Include(String), // path to another plan file
}

impl Plan {
    /// Return only the Step items (useful after resolve_includes has flattened everything).
    pub fn steps(&self) -> Vec<&Step> {
        self.items
            .iter()
            .filter_map(|item| match item {
                PlanItem::Step(s) => Some(s),
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
}

#[derive(Debug, Clone)]
pub struct TaskDef {
    pub module: String,
    pub resource: String,
    pub args: HashMap<String, ParamValue>,
    pub register: Option<String>,
    /// Module-level escalation override (most specific).
    pub run_as: RunAsSpec,
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
