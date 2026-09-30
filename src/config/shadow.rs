//! Plan variables that replace what the inventory sets for a host.
//!
//! Plan vars merge last, so a value a plan meant as a default beats what the inventory
//! says about a specific host — silently, unless it is reported. Only names are compared
//! and reported: either value may be a secret.

use crate::config::types::{Inventory, Plan};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Where an inventory sets a variable for a host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum VarScope {
    SecretsFile,
    Global,
    Group(String),
    Host(String),
}

impl std::fmt::Display for VarScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VarScope::SecretsFile => write!(f, "the secrets file"),
            VarScope::Global => write!(f, "the inventory's global vars"),
            VarScope::Group(name) => write!(f, "group '{name}'"),
            VarScope::Host(name) => write!(f, "host '{name}'"),
        }
    }
}

/// A plan variable that replaces what the inventory sets for hosts running the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadow {
    pub plan: String,
    /// The plan file it was found in: two files may name their plans alike.
    pub source: PathBuf,
    pub name: String,
    /// Every place the inventory sets it for those hosts.
    pub scopes: BTreeSet<VarScope>,
}

impl Shadow {
    pub fn warning(&self) -> String {
        let scopes: Vec<String> = self.scopes.iter().map(ToString::to_string).collect();
        format!(
            "plan '{}' overrides '{}', which is also set by {}: the plan's value wins, as \
             plan vars merge last",
            self.plan,
            self.name,
            join_and(&scopes)
        )
    }
}

fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The names a secrets file defines: scalars, and structured (list) values.
#[derive(Debug, Default)]
pub struct SecretNames {
    pub vars: HashSet<String>,
    pub structured: HashSet<String>,
}

/// The plan's variables — its own, its includes', and the `vars-prompt` names that answer
/// into them — that the inventory, or the secrets file, also sets for any of `hosts`.
/// Structured variables only meet the secrets file's: the inventory has none.
///
/// `inventory` is the inventory as written, before secrets-file values are merged into
/// its global vars, and `plan` before the secrets file's structured values are merged into
/// it — after that, whose they were is lost. Without an inventory, a run targets a host
/// given on the command line, and only the secrets file can be shadowed.
pub fn shadowed<'a>(
    plan: &Plan,
    source: &Path,
    inventory: Option<&Inventory>,
    secrets: &SecretNames,
    hosts: impl IntoIterator<Item = &'a str>,
) -> Vec<Shadow> {
    let hosts: HashSet<&str> = hosts.into_iter().collect();
    let mut scopes: BTreeMap<&str, BTreeSet<VarScope>> = BTreeMap::new();
    for name in &secrets.vars {
        scopes
            .entry(name.as_str())
            .or_default()
            .insert(VarScope::SecretsFile);
    }
    if let Some(inventory) = inventory {
        let grouped = inventory
            .groups
            .iter()
            .flat_map(|g| g.hosts.iter().map(move |h| (Some(g), h)));
        let ungrouped = inventory.ungrouped_hosts.iter().map(|h| (None, h));
        for (group, host) in grouped.chain(ungrouped) {
            if !hosts.contains(host.name.as_str()) {
                continue;
            }
            for name in inventory.global_vars.keys() {
                scopes.entry(name).or_default().insert(VarScope::Global);
            }
            if let Some(group) = group {
                for name in group.vars.keys() {
                    scopes
                        .entry(name)
                        .or_default()
                        .insert(VarScope::Group(group.name.clone()));
                }
            }
            for name in host.vars.keys() {
                scopes
                    .entry(name)
                    .or_default()
                    .insert(VarScope::Host(host.name.clone()));
            }
        }
    }

    let plan_names: BTreeSet<&str> = plan
        .vars
        .keys()
        .map(String::as_str)
        .chain(plan.prompts.iter().map(|p| p.name.as_str()))
        .collect();
    let structured = plan
        .structured_vars
        .keys()
        .filter(|name| secrets.structured.contains(*name))
        .map(|name| (name.as_str(), BTreeSet::from([VarScope::SecretsFile])));
    let scalars = plan_names
        .into_iter()
        .filter_map(|name| scopes.get(name).map(|found| (name, found.clone())));
    // One file named two ways (`plan.kdl`, `sub/../plan.kdl`, a link) is one plan.
    let source = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    merged(scalars.chain(structured).map(|(name, scopes)| Shadow {
        plan: plan.name.clone(),
        source: source.clone(),
        name: name.to_string(),
        scopes,
    }))
}

/// One shadow per plan file and variable, with the scopes of all: a plan several
/// inventory groups run is checked once per group.
pub fn merged(shadows: impl IntoIterator<Item = Shadow>) -> Vec<Shadow> {
    let mut by_source: BTreeMap<(PathBuf, String), Shadow> = BTreeMap::new();
    for shadow in shadows {
        match by_source.entry((shadow.source.clone(), shadow.name.clone())) {
            std::collections::btree_map::Entry::Occupied(mut known) => {
                known.get_mut().scopes.extend(shadow.scopes)
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(shadow);
            }
        }
    }
    by_source.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse_inventory, parse_plan};

    const INVENTORY: &str = r#"
vars {
    region "eu"
}
group "web" {
    vars {
        customer "acme"
    }
    host "web-1" "10.0.0.1" {
        vars {
            tier "gold"
        }
    }
    host "web-2" "10.0.0.2"
}
group "db" {
    vars {
        backup "nightly"
    }
    host "db-1" "10.0.1.1"
}
host "lone" "10.0.2.1" {
    vars {
        customer "other"
    }
}
"#;

    fn shadows(plan: &str, secrets: &[&str], hosts: &[&str]) -> Vec<Shadow> {
        let inventory = parse_inventory(INVENTORY).unwrap();
        let plan = parse_plan(plan).unwrap();
        let secrets = SecretNames {
            vars: secrets.iter().map(|s| s.to_string()).collect(),
            ..SecretNames::default()
        };
        shadowed(
            &plan,
            Path::new("plan.kdl"),
            Some(&inventory),
            &secrets,
            hosts.iter().copied(),
        )
    }

    fn scopes(shadow: &Shadow) -> Vec<VarScope> {
        shadow.scopes.iter().cloned().collect()
    }

    #[test]
    fn a_plan_var_the_inventory_also_sets_is_reported_with_every_scope() {
        let found = shadows(
            r#"plan "deploy" {
                vars {
                    customer "default"
                    region "us"
                    tier "silver"
                    fresh "x"
                }
                step "s" { shell "true" }
            }"#,
            &[],
            &["web-1", "lone"],
        );
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["customer", "region", "tier"]);
        assert_eq!(
            scopes(&found[0]),
            [
                VarScope::Group("web".to_string()),
                VarScope::Host("lone".to_string())
            ]
        );
        assert_eq!(scopes(&found[1]), [VarScope::Global]);
        assert_eq!(scopes(&found[2]), [VarScope::Host("web-1".to_string())]);
    }

    #[test]
    fn only_the_hosts_running_the_plan_count() {
        let plan = r#"plan "p" {
            vars { backup "weekly"; tier "x" }
            step "s" { shell "true" }
        }"#;
        assert!(shadows(plan, &[], &["web-2"]).is_empty());
        let found = shadows(plan, &[], &["db-1"]);
        assert_eq!(found.len(), 1);
        assert_eq!(scopes(&found[0]), [VarScope::Group("db".to_string())]);
    }

    #[test]
    fn a_secrets_file_value_and_a_prompted_name_count() {
        let found = shadows(
            r#"plan "p" {
                vars-prompt { customer "Which customer?" }
                vars { token "placeholder" }
                step "s" { shell "true" }
            }"#,
            &["token"],
            &["web-2"],
        );
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["customer", "token"]);
        assert_eq!(scopes(&found[1]), [VarScope::SecretsFile]);
    }

    #[test]
    fn without_an_inventory_only_the_secrets_file_counts() {
        let plan = parse_plan(
            r#"plan "p" { vars { token "x"; region "us" }
                step "s" { shell "true" } }"#,
        )
        .unwrap();
        let secrets = SecretNames {
            vars: HashSet::from(["token".to_string()]),
            ..SecretNames::default()
        };
        let found = shadowed(&plan, Path::new("plan.kdl"), None, &secrets, ["10.0.0.9"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "token");
    }

    #[test]
    fn the_warning_names_the_variable_and_its_scopes_never_a_value() {
        let found = shadows(
            r#"plan "deploy" { vars { customer "default-customer" }
                step "s" { shell "true" } }"#,
            &[],
            &["web-1", "lone"],
        );
        let warning = found[0].warning();
        assert_eq!(
            warning,
            "plan 'deploy' overrides 'customer', which is also set by group 'web' and host \
             'lone': the plan's value wins, as plan vars merge last"
        );
        assert!(!warning.contains("default-customer") && !warning.contains("acme"));
    }

    #[test]
    fn a_structured_plan_var_the_secrets_file_also_defines_counts() {
        let plan = parse_plan(
            r#"plan "p" {
                vars {
                    api-keys {
                        - name="a" value="x"
                    }
                    other {
                        - name="b"
                    }
                }
                step "s" { shell "true" }
            }"#,
        )
        .unwrap();
        let secrets = SecretNames {
            vars: HashSet::from(["other".to_string()]),
            structured: HashSet::from(["api-keys".to_string()]),
        };
        let found = shadowed(&plan, Path::new("plan.kdl"), None, &secrets, ["web-1"]);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["api-keys"],
            "a scalar and a list of one name do not meet"
        );
        assert_eq!(scopes(&found[0]), [VarScope::SecretsFile]);
    }

    #[test]
    fn shadows_of_one_plan_file_merge_and_of_two_files_do_not() {
        let shadow = |source: &str, scope: VarScope| Shadow {
            plan: "deploy".to_string(),
            source: PathBuf::from(source),
            name: "customer".to_string(),
            scopes: BTreeSet::from([scope]),
        };
        let found = merged([
            shadow("web.kdl", VarScope::Group("web".to_string())),
            shadow("web.kdl", VarScope::Host("db-1".to_string())),
            shadow("other.kdl", VarScope::Host("lone".to_string())),
        ]);
        assert_eq!(found.len(), 2, "{found:?}");
        let web = found
            .iter()
            .find(|s| s.source == Path::new("web.kdl"))
            .unwrap();
        assert_eq!(
            scopes(web),
            [
                VarScope::Group("web".to_string()),
                VarScope::Host("db-1".to_string())
            ]
        );
        let other = found
            .iter()
            .find(|s| s.source == Path::new("other.kdl"))
            .unwrap();
        assert_eq!(scopes(other), [VarScope::Host("lone".to_string())]);
    }

    #[test]
    fn one_plan_file_named_two_ways_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("plan.kdl"), "").unwrap();
        let inventory = parse_inventory(INVENTORY).unwrap();
        let plan = parse_plan(
            r#"plan "deploy" { vars { customer "x" }
                step "s" { shell "true" } }"#,
        )
        .unwrap();
        let secrets = SecretNames::default();
        let direct = dir.path().join("plan.kdl");
        let aliased = dir.path().join("sub/../plan.kdl");
        let found = merged(
            shadowed(&plan, &direct, Some(&inventory), &secrets, ["web-1"])
                .into_iter()
                .chain(shadowed(
                    &plan,
                    &aliased,
                    Some(&inventory),
                    &secrets,
                    ["lone"],
                )),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            scopes(&found[0]),
            [
                VarScope::Group("web".to_string()),
                VarScope::Host("lone".to_string())
            ]
        );
    }
}
