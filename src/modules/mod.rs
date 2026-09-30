pub mod container;
pub mod context;
pub mod detect;
pub mod disk;
pub mod escalation;
pub mod external;
pub mod file;
pub mod file_diff;
pub mod file_tree;
pub mod host;
pub mod nix;
pub mod package;
pub mod shell;
pub mod systemd;
pub mod user;

use crate::config::types::ParamValue;
use crate::error::GlideshError;
use async_trait::async_trait;
use context::ModuleContext;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum ModuleStatus {
    Satisfied,
    Pending {
        plan: String,
        /// Set only under `--diff`, by modules able to describe the change in detail.
        diff: Option<String>,
    },
    Unknown {
        reason: String,
    },
}

impl ModuleStatus {
    pub fn pending(plan: impl Into<String>) -> Self {
        ModuleStatus::Pending {
            plan: plan.into(),
            diff: None,
        }
    }

    pub fn pending_with_diff(plan: impl Into<String>, diff: impl Into<String>) -> Self {
        ModuleStatus::Pending {
            plan: plan.into(),
            diff: Some(diff.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModuleResult {
    pub changed: bool,
    pub output: String,
    pub stderr: String,
    pub exit_code: i32,
    /// `output` is a command's stdout that went over
    /// [`OUTPUT_LIMIT`](crate::ssh::connection::OUTPUT_LIMIT) and lost its middle, so
    /// `register=` must not capture it.
    pub output_cut: bool,
}

#[derive(Debug, Clone)]
pub struct ModuleParams {
    pub resource_name: String,
    pub args: HashMap<String, ParamValue>,
}

#[async_trait]
pub trait Module: Send + Sync {
    fn name(&self) -> &str;

    /// Every parameter the module reads, so a plan naming any other fails before
    /// connecting instead of being ignored; `None` when glidesh cannot know them (a plugin).
    fn params(&self) -> Option<&'static [&'static str]>;

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError>;

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError>;
}

pub struct ModuleRegistry {
    modules: HashMap<String, Box<dyn Module>>,
    external_modules: HashMap<String, Box<dyn Module>>,
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleRegistry {
    pub fn new() -> Self {
        let mut registry = ModuleRegistry {
            modules: HashMap::new(),
            external_modules: HashMap::new(),
        };
        registry.register_builtin(Box::new(shell::ShellModule));
        registry.register_builtin(Box::new(package::PackageModule));
        registry.register_builtin(Box::new(user::UserModule));
        registry.register_builtin(Box::new(systemd::SystemdModule));
        registry.register_builtin(Box::new(container::ContainerModule));
        registry.register_builtin(Box::new(file::FileModule));
        registry.register_builtin(Box::new(disk::DiskModule));
        registry.register_builtin(Box::new(nix::NixModule));
        registry
    }

    /// Names of the built-in modules, in no particular order. `host` is not among them: the
    /// executor runs it itself.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.modules.keys().map(String::as_str)
    }

    pub fn with_external(inventory_dir: Option<&Path>) -> Self {
        let mut registry = Self::new();

        let external = external::discovery::discover_external_modules(inventory_dir);

        for info in external {
            if registry.external_modules.contains_key(&info.name) {
                tracing::debug!(
                    "Skipping duplicate external module '{}' at '{}'",
                    info.name,
                    info.path.display()
                );
                continue;
            }
            tracing::info!(
                "Loaded external module '{}' v{} from '{}'",
                info.name,
                info.version,
                info.path.display()
            );
            registry.external_modules.insert(
                info.name.clone(),
                Box::new(external::runner::ExternalModule::new(info)),
            );
        }

        registry
    }

    fn register_builtin(&mut self, module: Box<dyn Module>) {
        self.modules.insert(module.name().to_string(), module);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Module> {
        if let Some(ext_name) = name.strip_prefix("external.") {
            self.external_modules.get(ext_name).map(|m| m.as_ref())
        } else {
            self.modules.get(name).map(|m| m.as_ref())
        }
    }

    /// Fails with every problem [`Self::plan_problems`] finds, one per line.
    pub fn validate_plan(
        &self,
        plan: &crate::config::types::Plan,
    ) -> Result<(), crate::error::GlideshError> {
        let problems = self.plan_problems(plan);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(crate::error::GlideshError::ConfigParse {
                message: problems.join("\n"),
            })
        }
    }

    /// Modules the registry does not have, and parameters a built-in module does not read.
    pub fn plan_problems(&self, plan: &crate::config::types::Plan) -> Vec<String> {
        let mut missing = Vec::new();
        let mut problems = Vec::new();
        for step in plan.steps() {
            for task in step.all_tasks() {
                // `host` is not in the registry — it's intercepted directly
                // by NodeRunner and routed through HostCoordinator for
                // run-once-share-to-all semantics.
                let accepted = if task.module == host::MODULE_NAME {
                    Some(host::PARAMS)
                } else if let Some(module) = self.get(&task.module) {
                    module.params()
                } else {
                    missing.push(task.module.clone());
                    continue;
                };
                if let Some(accepted) = accepted {
                    problems.extend(unknown_params(&step.name, task, accepted));
                }
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            let display: Vec<String> = missing
                .into_iter()
                .map(|m| {
                    if let Some(name) = m.strip_prefix("external.") {
                        format!("external \"{name}\"")
                    } else {
                        m
                    }
                })
                .collect();
            problems.insert(0, format!("Unknown module(s): {}", display.join(", ")));
        }
        problems
    }
}

/// One line per parameter of `task` that `accepted` does not list, naming the closest
/// accepted one and all of them; a step attribute written on a task says where it goes.
fn unknown_params(
    step: &str,
    task: &crate::config::types::TaskDef,
    accepted: &[&str],
) -> Vec<String> {
    let mut unknown: Vec<&str> = task
        .args
        .keys()
        .map(String::as_str)
        .filter(|key| !accepted.contains(key))
        .collect();
    unknown.sort_unstable();
    unknown
        .into_iter()
        .map(|key| {
            let module = &task.module;
            let at = format!("step '{step}': {module} '{}'", task.resource);
            if crate::config::plan::STEP_ATTRS.contains(&key) {
                return format!(
                    "{at}: {key}= is a step attribute, not a {module} parameter; \
                     move it to the step"
                );
            }
            let hint = crate::config::prompts::closest(key, accepted.iter().copied())
                .map(|close| format!("did you mean '{close}'? "))
                .unwrap_or_default();
            format!(
                "{at}: unknown parameter '{key}' ({hint}{module} accepts: {})",
                accepted.join(", ")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems(registry: &ModuleRegistry, body: &str) -> Vec<String> {
        let plan =
            crate::config::parse_plan(&format!(r#"plan "p" {{ step "s" {{ {body} }} }}"#)).unwrap();
        registry.plan_problems(&plan)
    }

    #[test]
    fn every_builtin_rejects_a_parameter_it_does_not_read() {
        let registry = ModuleRegistry::new();
        let modules: Vec<&str> = registry
            .builtin_names()
            .chain([host::MODULE_NAME])
            .collect();
        assert!(modules.len() >= 9, "{modules:?}");
        for module in modules {
            let found = problems(&registry, &format!(r#"{module} "x" bogus="yes""#));
            assert_eq!(found.len(), 1, "{module}: {found:?}");
            assert!(
                found[0].starts_with(&format!(
                    "step 's': {module} 'x': unknown parameter 'bogus' ("
                )),
                "{}",
                found[0]
            );
        }
    }

    #[test]
    fn a_known_parameter_passes() {
        let registry = ModuleRegistry::new();
        let found = problems(
            &registry,
            r#"shell "make" check="test -f out" retries=2 delay=1 timeout=60 login=#true
               package "nginx" state="absent"
               file "/etc/a" src="a" template=#true mode="0644" diff=#false
               host "tag" cmd="date" on="web-1"
               container "app" image="nginx" memory="512m" wait="healthy""#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_misspelled_parameter_names_the_closest_and_all_accepted() {
        let found = problems(
            &ModuleRegistry::new(),
            r#"file "/etc/a" src="a" mdoe="0644""#,
        );
        assert_eq!(
            found,
            [
                "step 's': file '/etc/a': unknown parameter 'mdoe' (did you mean 'mode'? file accepts: diff, dir-mode, exclude, fetch, file-mode, group, mode, owner, prune, recurse, src, template)"
            ]
        );
    }

    #[test]
    fn a_step_attribute_on_a_task_says_where_it_goes() {
        let found = problems(&ModuleRegistry::new(), r#"shell "true" loop="${xs}""#);
        assert_eq!(
            found,
            [
                "step 's': shell 'true': loop= is a step attribute, not a shell parameter; move it to the step"
            ]
        );
    }

    #[test]
    fn rescue_and_always_tasks_are_checked() {
        let found = problems(
            &ModuleRegistry::new(),
            r#"shell "false"; rescue { shell "a" bogus=1 }; always { package "b" bogus=1 }"#,
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_plugin_takes_any_parameter() {
        let mut registry = ModuleRegistry::new();
        registry.external_modules.insert(
            "acme".to_string(),
            Box::new(external::runner::ExternalModule::new(
                external::discovery::ExternalModuleInfo {
                    name: "acme".to_string(),
                    path: "acme".into(),
                    version: "1".to_string(),
                    interpreter: None,
                },
            )),
        );
        let found = problems(&registry, r#"external "acme" "x" anything="yes""#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_unknown_module_comes_first_and_its_parameters_are_not_checked() {
        let found = problems(
            &ModuleRegistry::new(),
            r#"pakage "nginx" bogus=1; shell "true" bogus=1"#,
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], "Unknown module(s): pakage");
        assert!(found[1].starts_with("step 's': shell"), "{}", found[1]);
    }
}
