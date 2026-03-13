use std::path::{Path, PathBuf};

const MODULE_PREFIX: &str = "glidesh-module-";

#[derive(Debug, Clone)]
pub struct ExternalModuleInfo {
    pub name: String,
    pub path: PathBuf,
    pub version: String,
    pub interpreter: Option<String>,
}

pub fn discover_external_modules(inventory_dir: Option<&Path>) -> Vec<ExternalModuleInfo> {
    let mut modules = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    if let Some(dir) = inventory_dir {
        let modules_dir = dir.join("modules");
        scan_directory(&modules_dir, &mut modules, &mut seen_names);
    }

    if let Some(home) = dirs::home_dir() {
        let user_dir = home.join(".glidesh").join("modules");
        scan_directory(&user_dir, &mut modules, &mut seen_names);
    }

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

fn try_parse_module(
    path: &Path,
    seen: &mut std::collections::HashSet<String>,
) -> Option<ExternalModuleInfo> {
    if !path.is_file() {
        return None;
    }

    let file_name = path.file_name()?.to_string_lossy();

    let base_name = file_name.strip_suffix(".exe").unwrap_or(&file_name);
    base_name.strip_prefix(MODULE_PREFIX)?;

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

fn parse_shebang(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;

    let shebang = first_line.strip_prefix("#!")?;
    let shebang = shebang.trim();

    let interpreter = if let Some(rest) = shebang.strip_prefix("/usr/bin/env ") {
        rest.split_whitespace().next()?
    } else {
        shebang.split_whitespace().next()?
    };

    let basename = Path::new(interpreter).file_name()?.to_str()?;

    Some(basename.to_string())
}

pub fn build_tokio_command(info: &ExternalModuleInfo) -> tokio::process::Command {
    if let Some(ref interp) = info.interpreter {
        let mut cmd = tokio::process::Command::new(interp);
        cmd.arg(&info.path);
        cmd
    } else {
        tokio::process::Command::new(&info.path)
    }
}

fn build_probe_command(path: &Path, interpreter: Option<&str>) -> std::process::Command {
    if let Some(interp) = interpreter {
        let mut cmd = std::process::Command::new(interp);
        cmd.arg(path);
        cmd
    } else {
        std::process::Command::new(path)
    }
}

fn probe_module(path: &Path) -> Result<ExternalModuleInfo, String> {
    use std::io::{BufRead, Write};
    use std::process::Stdio;

    let interpreter = if cfg!(windows) {
        parse_shebang(path)
    } else {
        None
    };

    let mut child = build_probe_command(path, interpreter.as_deref())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;

    writeln!(stdin, r#"{{"method":"describe"}}"#).map_err(|e| format!("write failed: {}", e))?;
    stdin.flush().map_err(|e| format!("flush failed: {}", e))?;

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

    let cleanup = |mut stdin: std::process::ChildStdin,
                   mut child: std::process::Child,
                   reader_thread: std::thread::JoinHandle<_>| {
        let _ = writeln!(stdin, r#"{{"method":"shutdown"}}"#);
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader_thread.join();
    };

    let response = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(line)) => line,
        Ok(Err(e)) => {
            cleanup(stdin, child, reader_thread);
            return Err(format!("read failed: {}", e));
        }
        Err(_) => {
            cleanup(stdin, child, reader_thread);
            return Err("describe timed out".to_string());
        }
    };

    cleanup(stdin, child, reader_thread);

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
        interpreter,
    })
}
