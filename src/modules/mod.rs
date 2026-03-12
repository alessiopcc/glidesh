pub mod container;
pub mod context;
pub mod detect;
pub mod disk;
pub mod file;
pub mod package;
pub mod shell;
pub mod systemd;
pub mod user;

use crate::config::types::ParamValue;
use crate::error::GlideshError;
use async_trait::async_trait;
use context::ModuleContext;
use std::collections::HashMap;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ModuleStatus {
    Satisfied,
    Pending { plan: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
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
        };
        registry.register(Box::new(shell::ShellModule));
        registry.register(Box::new(package::PackageModule));
        registry.register(Box::new(user::UserModule));
        registry.register(Box::new(systemd::SystemdModule));
        registry.register(Box::new(container::ContainerModule));
        registry.register(Box::new(file::FileModule));
        registry.register(Box::new(disk::DiskModule));
        registry
    }

    pub fn register(&mut self, module: Box<dyn Module>) {
        self.modules.insert(module.name().to_string(), module);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Module> {
        self.modules.get(name).map(|m| m.as_ref())
    }
}
