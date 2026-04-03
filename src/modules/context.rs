use crate::config::template::TemplateData;
use crate::modules::detect::OsInfo;
use crate::ssh::SshSession;
use std::collections::HashMap;
use std::path::Path;

pub struct ModuleContext<'a> {
    pub ssh: &'a SshSession,
    pub os_info: &'a OsInfo,
    pub vars: &'a HashMap<String, String>,
    pub template_data: &'a TemplateData,
    pub dry_run: bool,
    pub plan_base_dir: &'a Path,
}
