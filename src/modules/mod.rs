pub mod container;
pub mod context;
pub mod detect;
pub mod disk;
pub mod external;
pub mod file;
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
    Pending { plan: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone)]
pub struct ModuleResult {
    pub changed: bool,
    pub output: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub struct ModuleParams {
    pub resource_name: String,
    pub args: HashMap<String, ParamValue>,
}

#[async_trait]
pub trait Module: Send + Sync {
    fn name(&self) -> &str;

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

    pub fn validate_plan(
        &self,
        plan: &crate::config::types::Plan,
    ) -> Result<(), crate::error::GlideshError> {
        let mut missing = Vec::new();
        for step in plan.steps() {
            for task in &step.tasks {
                // `host` is not in the registry — it's intercepted directly
                // by NodeRunner and routed through HostCoordinator for
                // run-once-share-to-all semantics.
                if task.module == host::MODULE_NAME {
                    continue;
                }
                if self.get(&task.module).is_none() {
                    missing.push(task.module.clone());
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
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
            Err(crate::error::GlideshError::ConfigParse {
                message: format!("Unknown module(s): {}", display.join(", ")),
            })
        }
    }
}
