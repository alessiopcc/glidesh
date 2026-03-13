use std::path::{Path, PathBuf};

const MODULE_PREFIX: &str = "glidesh-module-";

#[derive(Debug, Clone)]
pub struct ExternalModuleInfo {
    pub name: String,
    pub path: PathBuf,
    pub version: String,
}

/// Discover external modules from conventional locations and extra paths.
///
/// Search order:
/// 1. `plan_dir/modules/` (if plan_dir is provided)
/// 2. `~/.glidesh/modules/`
/// 3. Extra paths from `--module-path`
/// 4. `$PATH` executables matching `glidesh-module-*`
pub fn discover_external_modules(
    plan_dir: Option<&Path>,
    extra_paths: &[PathBuf],
) -> Vec<ExternalModuleInfo> {
    let mut modules = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // 1. Plan-local modules directory
    if let Some(dir) = plan_dir {
        let modules_dir = dir.join("modules");
        scan_directory(&modules_dir, &mut modules, &mut seen_names);
    }

    // 2. User-global modules directory
    if let Some(home) = dirs::home_dir() {
        let user_dir = home.join(".glidesh").join("modules");
        scan_directory(&user_dir, &mut modules, &mut seen_names);
    }

    // 3. Extra paths from --module-path
    for dir in extra_paths {
        scan_directory(dir, &mut modules, &mut seen_names);
    }

    // 4. $PATH scan
    scan_path_env(&mut modules, &mut seen_names);

    modules
}

fn scan_directory(
    dir: &Path,
    modules: &mut Vec<ExternalModuleInfo>,
    seen: &mut std::collections::HashSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(info) = try_parse_module(&path, seen) {
            modules.push(info);
        }
    }
}

fn scan_path_env(
    modules: &mut Vec<ExternalModuleInfo>,
    seen: &mut std::collections::HashSet<String>,
) {
    let path_var = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return,
    };

    for dir in std::env::split_paths(&path_var) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(info) = try_parse_module(&path, seen) {
                modules.push(info);
            }
        }
    }
}

fn try_parse_module(
    path: &Path,
    seen: &mut std::collections::HashSet<String>,
) -> Option<ExternalModuleInfo> {
    if !path.is_file() {
        return None;
    }

    let file_name = path.file_name()?.to_string_lossy();

    // Strip .exe suffix on Windows
    let base_name = file_name.strip_suffix(".exe").unwrap_or(&file_name);

    // Verify the executable matches the naming convention
    base_name.strip_prefix(MODULE_PREFIX)?;

    // Probe the module to get the canonical name from its describe response
    match probe_module(path) {
        Ok(info) => {
            if info.name.is_empty() {
                tracing::warn!(
                    "External module at '{}' returned empty name, skipping",
                    path.display()
                );
                return None;
            }
            if seen.contains(&info.name) {
                return None;
            }
            seen.insert(info.name.clone());
            Some(info)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to probe external module at '{}': {}",
                path.display(),
                e
            );
            None
        }
    }
}

fn probe_module(path: &Path) -> Result<ExternalModuleInfo, String> {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;

    // Send describe request
    writeln!(stdin, r#"{{"method":"describe"}}"#).map_err(|e| format!("write failed: {}", e))?;
    stdin.flush().map_err(|e| format!("flush failed: {}", e))?;

    // Read response with a short timeout via a thread
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => tx.send(Err("EOF".to_string())),
            Ok(_) => tx.send(Ok(line)),
            Err(e) => tx.send(Err(e.to_string())),
        }
    });

    let response = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "describe timed out".to_string())?
        .map_err(|e| format!("read failed: {}", e))?;

    // Send shutdown and clean up
    let _ = writeln!(stdin, r#"{{"method":"shutdown"}}"#);
    let _ = child.kill();
    let _ = reader_thread.join();

    let desc: super::protocol::DescribeResponse =
        serde_json::from_str(&response).map_err(|e| format!("invalid describe response: {}", e))?;

    if desc.protocol_version != super::protocol::PROTOCOL_VERSION {
        return Err(format!(
            "unsupported protocol version {} (expected {})",
            desc.protocol_version,
            super::protocol::PROTOCOL_VERSION
        ));
    }

    Ok(ExternalModuleInfo {
        name: desc.name,
        path: path.to_path_buf(),
        version: desc.version,
    })
}
