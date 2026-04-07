use crate::config::types::{Group, Host, Inventory, JumpHost};
use crate::error::GlideshError;
use std::collections::HashMap;

pub fn parse_inventory(input: &str) -> Result<Inventory, GlideshError> {
    let doc: kdl::KdlDocument = input.parse().map_err(|e: kdl::KdlError| {
        let details = super::format_kdl_error(input, &e);
        GlideshError::ConfigParse {
            message: format!("Failed to parse inventory KDL:\n{}", details),
        }
    })?;

    let mut global_vars = HashMap::new();
    let mut groups = Vec::new();
    let mut ungrouped_hosts = Vec::new();

    for node in doc.nodes() {
        match node.name().to_string().as_str() {
            "vars" => {
                if let Some(children) = node.children() {
                    global_vars = parse_vars_block(children)?;
                }
            }
            "group" => {
                groups.push(parse_group(node)?);
            }
            "host" => {
                ungrouped_hosts.push(parse_host(node)?);
            }
            other => {
                return Err(GlideshError::ConfigParse {
                    message: format!("Unknown top-level node in inventory: '{}'", other),
                });
            }
        }
    }

    // Validate: no duplicate hostnames across the entire inventory
    let mut seen_hosts = std::collections::HashSet::new();
    for group in &groups {
        for host in &group.hosts {
            if !seen_hosts.insert(&host.name) {
                return Err(GlideshError::ConfigParse {
                    message: format!("Duplicate host name '{}' in inventory", host.name),
                });
            }
        }
    }
    for host in &ungrouped_hosts {
        if !seen_hosts.insert(&host.name) {
            return Err(GlideshError::ConfigParse {
                message: format!("Duplicate host name '{}' in inventory", host.name),
            });
        }
    }

    // Validate: group names must not collide with ungrouped host names
    let group_names: std::collections::HashSet<&str> =
        groups.iter().map(|g| g.name.as_str()).collect();
    for host in &ungrouped_hosts {
        if group_names.contains(host.name.as_str()) {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "Ungrouped host '{}' has the same name as a group. \
                     Consider adding it to a group instead.",
                    host.name
                ),
            });
        }
    }

    Ok(Inventory {
        groups,
        ungrouped_hosts,
        global_vars,
    })
}

enum NameKind {
    Group,
    Host,
}

impl std::fmt::Display for NameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameKind::Group => write!(f, "Group"),
            NameKind::Host => write!(f, "Host"),
        }
    }
}

fn validate_name(name: &str, kind: NameKind) -> Result<(), GlideshError> {
    if name.is_empty() {
        return Err(GlideshError::ConfigParse {
            message: format!("{} name cannot be empty", kind),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(GlideshError::ConfigParse {
            message: format!(
                "{} name '{}' contains invalid characters (only letters, digits, '-' and '_' are allowed)",
                kind, name
            ),
        });
    }
    Ok(())
}

fn parse_group(node: &kdl::KdlNode) -> Result<Group, GlideshError> {
    let name = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "Group node requires a name argument".to_string(),
        })?
        .to_string();

    validate_name(&name, NameKind::Group)?;

    let plan = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("plan"))
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let mut hosts = Vec::new();
    let mut vars = HashMap::new();
    let mut jump = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "host" => hosts.push(parse_host(child)?),
                "vars" => {
                    if let Some(vc) = child.children() {
                        vars = parse_vars_block(vc)?;
                    }
                }
                "jump" => {
                    jump = Some(parse_jump(child)?);
                }
                other => {
                    return Err(GlideshError::ConfigParse {
                        message: format!("Unknown node in group '{}': '{}'", name, other),
                    });
                }
            }
        }
    }

    Ok(Group {
        name,
        hosts,
        vars,
        plan,
        jump,
    })
}

fn parse_host(node: &kdl::KdlNode) -> Result<Host, GlideshError> {
    let args: Vec<&str> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .filter_map(|e| e.value().as_string())
        .collect();

    if args.len() < 2 {
        return Err(GlideshError::ConfigParse {
            message: format!(
                "Host node requires name and address arguments, got {} args",
                args.len()
            ),
        });
    }

    let name = args[0].to_string();
    validate_name(&name, NameKind::Host)?;
    let address = args[1].to_string();

    let user = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("user"))
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let port = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("port"))
        .and_then(|e| e.value().as_integer())
        .map(|p| p as u16);

    let plan = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("plan"))
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let mut jump = None;
    let mut vars = HashMap::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().to_string().as_str() {
                "jump" => {
                    jump = Some(parse_jump(child)?);
                }
                "vars" => {
                    if let Some(vc) = child.children() {
                        vars = parse_vars_block(vc)?;
                    }
                }
                other => {
                    return Err(GlideshError::ConfigParse {
                        message: format!("Unknown node in host '{}': '{}'", name, other),
                    });
                }
            }
        }
    }

    Ok(Host {
        name,
        address,
        user,
        port,
        vars,
        plan,
        jump,
    })
}

fn parse_jump(node: &kdl::KdlNode) -> Result<JumpHost, GlideshError> {
    let address = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| GlideshError::ConfigParse {
            message: "Jump node requires an address argument".to_string(),
        })?
        .to_string();

    let user = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("user"))
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let raw_port = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some("port"))
        .and_then(|e| e.value().as_integer());

    let port = match raw_port {
        Some(p) if (1..=65535).contains(&p) => Some(p as u16),
        Some(p) => {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "Invalid port {} in jump node; expected a value between 1 and 65535",
                    p
                ),
            });
        }
        None => None,
    };

    Ok(JumpHost {
        address,
        user,
        port,
    })
}

fn parse_vars_block(doc: &kdl::KdlDocument) -> Result<HashMap<String, String>, GlideshError> {
    let mut vars = HashMap::new();
    for node in doc.nodes() {
        let key = node.name().to_string();
        if vars.contains_key(&key) {
            return Err(GlideshError::ConfigParse {
                message: format!("Duplicate variable '{}' in vars block", key),
            });
        }
        let value = node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .map(|e| kdl_value_to_string(e.value()))
            .unwrap_or_default();
        vars.insert(key, value);
    }
    Ok(vars)
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
    fn test_parse_simple_inventory() {
        let input = r#"
vars {
    deploy-user "deploy"
    ssh-key "~/.ssh/id_ed25519"
}

group "web" {
    vars {
        http-port 8080
    }
    host "web-1" "10.0.0.1" user="deploy" port=22
    host "web-2" "10.0.0.2" user="deploy"
}

group "db" {
    host "db-1" "10.0.1.1" user="root" port=2222
}

host "monitoring" "10.0.2.1" user="admin"
"#;
        let inv = parse_inventory(input).unwrap();
        assert_eq!(inv.groups.len(), 2);
        assert_eq!(inv.groups[0].name, "web");
        assert_eq!(inv.groups[0].hosts.len(), 2);
        assert_eq!(inv.groups[0].hosts[0].name, "web-1");
        assert_eq!(inv.groups[0].hosts[0].address, "10.0.0.1");
        assert_eq!(inv.groups[0].hosts[0].port, Some(22));
        assert_eq!(inv.groups[1].name, "db");
        assert_eq!(inv.ungrouped_hosts.len(), 1);
        assert_eq!(inv.ungrouped_hosts[0].name, "monitoring");
        assert_eq!(inv.global_vars.get("deploy-user").unwrap(), "deploy");
    }

    #[test]
    fn test_resolve_targets_all() {
        let input = r#"
group "web" {
    host "web-1" "10.0.0.1"
    host "web-2" "10.0.0.2"
}
host "mon" "10.0.2.1"
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(None);
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn test_resolve_targets_group() {
        let input = r#"
group "web" {
    host "web-1" "10.0.0.1"
    host "web-2" "10.0.0.2"
}
group "db" {
    host "db-1" "10.0.1.1"
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("web"));
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "web-1");
    }

    #[test]
    fn test_group_plan_attribute() {
        let input = r#"
group "web" plan="web-plan.kdl" {
    host "web-1" "10.0.0.1"
    host "web-2" "10.0.0.2"
}

group "db" plan="db-plan.kdl" {
    host "db-1" "10.0.1.1"
}

group "cache" {
    host "cache-1" "10.0.3.1"
}

host "monitoring" "10.0.2.1" plan="mon-plan.kdl"
"#;
        let inv = parse_inventory(input).unwrap();
        assert_eq!(inv.groups[0].plan.as_deref(), Some("web-plan.kdl"));
        assert_eq!(inv.groups[1].plan.as_deref(), Some("db-plan.kdl"));
        assert_eq!(inv.groups[2].plan, None);
        assert_eq!(inv.ungrouped_hosts[0].plan.as_deref(), Some("mon-plan.kdl"));

        let group_plans = inv.resolve_group_plans();
        assert_eq!(group_plans.len(), 3); // web, db, monitoring
        assert_eq!(group_plans[0].0, "web");
        assert_eq!(group_plans[0].1, "web-plan.kdl");
        assert_eq!(group_plans[0].2.len(), 2);
        assert_eq!(group_plans[1].0, "db");
        assert_eq!(group_plans[1].2.len(), 1);
        assert_eq!(group_plans[2].0, "");
        assert_eq!(group_plans[2].1, "mon-plan.kdl");
        assert_eq!(group_plans[2].2.len(), 1);
    }

    #[test]
    fn test_var_inheritance() {
        let input = r#"
vars {
    deploy-user "global-user"
}
group "web" {
    vars {
        http-port 8080
    }
    host "web-1" "10.0.0.1"
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("web"));
        assert_eq!(resolved[0].user, "global-user");
        assert_eq!(resolved[0].vars.get("http-port").unwrap(), "8080");
    }

    #[test]
    fn test_jump_host_group_level() {
        let input = r#"
group "web" {
    jump "bastion.example.com" user="admin" port=2222
    host "web-1" "10.0.0.1" user="deploy"
    host "web-2" "10.0.0.2"
}
"#;
        let inv = parse_inventory(input).unwrap();
        let jump = inv.groups[0].jump.as_ref().unwrap();
        assert_eq!(jump.address, "bastion.example.com");
        assert_eq!(jump.user.as_deref(), Some("admin"));
        assert_eq!(jump.port, Some(2222));

        let resolved = inv.resolve_targets(Some("web"));
        let j1 = resolved[0].jump.as_ref().unwrap();
        assert_eq!(j1.address, "bastion.example.com");
        assert_eq!(j1.user, "admin");
        assert_eq!(j1.port, 2222);

        let j2 = resolved[1].jump.as_ref().unwrap();
        assert_eq!(j2.address, "bastion.example.com");
        assert_eq!(j2.user, "admin");
        assert_eq!(j2.port, 2222);
    }

    #[test]
    fn test_jump_host_per_host_override() {
        let input = r#"
group "web" {
    jump "group-bastion.example.com" user="groupuser"
    host "web-1" "10.0.0.1" user="deploy"
    host "web-2" "10.0.0.2" user="deploy" {
        jump "host-bastion.example.com" user="hostuser" port=3333
    }
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("web"));

        // web-1 inherits group jump
        let j1 = resolved[0].jump.as_ref().unwrap();
        assert_eq!(j1.address, "group-bastion.example.com");
        assert_eq!(j1.user, "groupuser");
        assert_eq!(j1.port, 22); // default

        // web-2 overrides with its own jump
        let j2 = resolved[1].jump.as_ref().unwrap();
        assert_eq!(j2.address, "host-bastion.example.com");
        assert_eq!(j2.user, "hostuser");
        assert_eq!(j2.port, 3333);
    }

    #[test]
    fn test_jump_host_inherits_user() {
        let input = r#"
group "web" {
    jump "bastion.example.com"
    host "web-1" "10.0.0.1" user="deploy"
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("web"));
        let j = resolved[0].jump.as_ref().unwrap();
        // jump user inherits from target host user
        assert_eq!(j.user, "deploy");
        assert_eq!(j.port, 22);
    }

    #[test]
    fn test_jump_host_ungrouped() {
        let input = r#"
host "standalone" "10.0.0.3" user="admin" {
    jump "bastion.example.com" port=2222
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("standalone"));
        let j = resolved[0].jump.as_ref().unwrap();
        assert_eq!(j.address, "bastion.example.com");
        assert_eq!(j.user, "admin"); // inherited from host
        assert_eq!(j.port, 2222);
    }

    #[test]
    fn test_no_jump_host() {
        let input = r#"
group "web" {
    host "web-1" "10.0.0.1"
}
host "standalone" "10.0.0.2"
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(None);
        assert!(resolved[0].jump.is_none());
        assert!(resolved[1].jump.is_none());
    }

    #[test]
    fn test_duplicate_hostname_same_group() {
        let input = r#"
group "web" {
    host "app" "10.0.0.1"
    host "app" "10.0.0.2"
}
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate host name 'app'"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_hostname_across_groups() {
        let input = r#"
group "web" {
    host "app" "10.0.0.1"
}
group "db" {
    host "app" "10.0.0.2"
}
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate host name 'app'"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_hostname_grouped_and_ungrouped() {
        let input = r#"
group "web" {
    host "app" "10.0.0.1"
}
host "app" "10.0.0.2"
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate host name 'app'"), "got: {}", msg);
    }

    #[test]
    fn test_ungrouped_host_collides_with_group_name() {
        let input = r#"
group "web" {
    host "web-1" "10.0.0.1"
}
host "web" "10.0.0.2"
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("same name as a group"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_var_in_inventory_vars_block() {
        let input = r#"
vars {
    region "us-east-1"
    region "eu-west-1"
}
host "web-1" "10.0.0.1"
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'region'"), "got: {}", msg);
    }

    #[test]
    fn test_duplicate_var_in_group_vars_block() {
        let input = r#"
group "web" {
    vars {
        port 8080
        port 9090
    }
    host "web-1" "10.0.0.1"
}
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'port'"), "got: {}", msg);
    }

    #[test]
    fn test_host_level_vars() {
        let input = r#"
vars {
    global-var "from-global"
}
group "web" {
    vars {
        http-port 8080
        group-var "from-group"
    }
    host "web-1" "10.0.0.1" {
        vars {
            http-port 9090
            host-var "from-host"
        }
    }
    host "web-2" "10.0.0.2"
}
"#;
        let inv = parse_inventory(input).unwrap();
        // Verify host-level vars are parsed
        assert_eq!(
            inv.groups[0].hosts[0].vars.get("http-port").unwrap(),
            "9090"
        );
        assert_eq!(
            inv.groups[0].hosts[0].vars.get("host-var").unwrap(),
            "from-host"
        );
        // web-2 has no host vars
        assert!(inv.groups[0].hosts[1].vars.is_empty());

        let resolved = inv.resolve_targets(Some("web"));
        // web-1: host var overrides group var
        assert_eq!(resolved[0].vars.get("http-port").unwrap(), "9090");
        assert_eq!(resolved[0].vars.get("host-var").unwrap(), "from-host");
        assert_eq!(resolved[0].vars.get("group-var").unwrap(), "from-group");
        assert_eq!(resolved[0].vars.get("global-var").unwrap(), "from-global");
        // web-2: inherits group and global, no host override
        assert_eq!(resolved[1].vars.get("http-port").unwrap(), "8080");
        assert_eq!(resolved[1].vars.get("group-var").unwrap(), "from-group");
        assert_eq!(resolved[1].vars.get("global-var").unwrap(), "from-global");
    }

    #[test]
    fn test_ungrouped_host_vars() {
        let input = r#"
host "standalone" "10.0.0.1" user="admin" {
    vars {
        env "production"
    }
}
"#;
        let inv = parse_inventory(input).unwrap();
        let resolved = inv.resolve_targets(Some("standalone"));
        assert_eq!(resolved[0].vars.get("env").unwrap(), "production");
    }

    #[test]
    fn test_duplicate_var_in_host_vars_block() {
        let input = r#"
group "web" {
    host "web-1" "10.0.0.1" {
        vars {
            port 8080
            port 9090
        }
    }
}
"#;
        let result = parse_inventory(input);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Duplicate variable 'port'"), "got: {}", msg);
    }
}
