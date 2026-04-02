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

#[derive(Debug, Clone)]
pub struct Host {
    pub name: String,
    pub address: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub vars: HashMap<String, String>,
    pub plan: Option<String>,
    pub jump: Option<JumpHost>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub hosts: Vec<Host>,
    pub vars: HashMap<String, String>,
    pub plan: Option<String>,
    pub jump: Option<JumpHost>,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub groups: Vec<Group>,
    pub ungrouped_hosts: Vec<Host>,
    pub global_vars: HashMap<String, String>,
}

impl Inventory {
    /// Resolve hosts matching a target filter (group name or host name).
    /// If target is None, return all hosts.
    pub fn resolve_targets(&self, target: Option<&str>) -> Vec<ResolvedHost> {
        let mut hosts = Vec::new();

        match target {
            None => {
                for group in &self.groups {
                    for host in &group.hosts {
                        hosts.push(self.resolve_host(host, Some(&group.vars), group.jump.as_ref()));
                    }
                }
                for host in &self.ungrouped_hosts {
                    hosts.push(self.resolve_host(host, None, None));
                }
            }
            Some(target) => {
                // Try group match first
                for group in &self.groups {
                    if group.name == target {
                        for host in &group.hosts {
                            hosts.push(self.resolve_host(
                                host,
                                Some(&group.vars),
                                group.jump.as_ref(),
                            ));
                        }
                        return hosts;
                    }
                }
                // Try host match
                for group in &self.groups {
                    for host in &group.hosts {
                        if host.name == target {
                            hosts.push(self.resolve_host(
                                host,
                                Some(&group.vars),
                                group.jump.as_ref(),
                            ));
                            return hosts;
                        }
                    }
                }
                for host in &self.ungrouped_hosts {
                    if host.name == target {
                        hosts.push(self.resolve_host(host, None, None));
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
                let hosts: Vec<ResolvedHost> = group
                    .hosts
                    .iter()
                    .map(|h| self.resolve_host(h, Some(&group.vars), group.jump.as_ref()))
                    .collect();
                if !hosts.is_empty() {
                    result.push((group.name.clone(), plan_path.clone(), hosts));
                }
            }
        }
        for host in &self.ungrouped_hosts {
            if let Some(ref plan_path) = host.plan {
                let resolved = self.resolve_host(host, None, None);
                result.push((String::new(), plan_path.clone(), vec![resolved]));
            }
        }
        result
    }

    fn resolve_host(
        &self,
        host: &Host,
        group_vars: Option<&HashMap<String, String>>,
        group_jump: Option<&JumpHost>,
    ) -> ResolvedHost {
        // Merge vars: global -> group -> host (most specific wins)
        let mut vars = self.global_vars.clone();
        if let Some(gv) = group_vars {
            vars.extend(gv.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        vars.extend(host.vars.iter().map(|(k, v)| (k.clone(), v.clone())));

        let user = host
            .user
            .clone()
            .or_else(|| vars.get("deploy-user").cloned())
            .unwrap_or_else(|| "root".to_string());

        let jump_source = host.jump.as_ref().or(group_jump);
        let jump = jump_source.map(|j| ResolvedJumpHost {
            address: j.address.clone(),
            user: j.user.clone().unwrap_or_else(|| user.clone()),
            port: j.port.unwrap_or(22),
        });

        ResolvedHost {
            name: host.name.clone(),
            address: host.address.clone(),
            user,
            port: host.port.unwrap_or(22),
            vars,
            jump,
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
}

#[derive(Debug, Clone)]
pub struct TaskDef {
    pub module: String,
    pub resource: String,
    pub args: HashMap<String, ParamValue>,
    pub register: Option<String>,
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
