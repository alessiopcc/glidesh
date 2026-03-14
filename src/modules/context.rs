use crate::modules::detect::OsInfo;
use crate::ssh::SshSession;
use std::collections::HashMap;

pub struct ModuleContext<'a> {
    pub ssh: &'a SshSession,
    pub os_info: &'a OsInfo,
    pub vars: &'a HashMap<String, String>,
    pub dry_run: bool,
}
