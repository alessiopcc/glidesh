use glidesh::ssh::tunnel::TunnelDirection;
use std::fs;
use std::path::{Path, PathBuf};

const STORE_FILENAME: &str = ".glidesh-tunnels.kdl";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTunnel {
    pub direction: TunnelDirection,
    pub via: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

fn store_path(inventory_path: &Path) -> PathBuf {
    inventory_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(STORE_FILENAME)
}

pub fn load(inventory_path: &Path) -> Vec<SavedTunnel> {
    let path = store_path(inventory_path);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let doc: kdl::KdlDocument = match content.parse() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to parse {}: {}", path.display(), e);
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for node in doc.nodes() {
        if node.name().to_string() != "tunnel" {
            continue;
        }
        let direction = match entry_str(node, "direction").as_deref() {
            Some("L") | Some("local") => TunnelDirection::Local,
            Some("R") | Some("reverse") => TunnelDirection::Reverse,
            _ => continue,
        };
        let via = match entry_str(node, "via") {
            Some(v) => v,
            None => continue,
        };
        let local_port = match entry_u16(node, "local-port") {
            Some(v) => v,
            None => continue,
        };
        let remote_host = entry_str(node, "remote-host").unwrap_or_else(|| "127.0.0.1".to_string());
        let remote_port = match entry_u16(node, "remote-port") {
            Some(v) => v,
            None => continue,
        };
        out.push(SavedTunnel {
            direction,
            via,
            local_port,
            remote_host,
            remote_port,
        });
    }
    out
}

pub fn save(inventory_path: &Path, specs: &[SavedTunnel]) -> std::io::Result<()> {
    let path = store_path(inventory_path);
    if specs.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let mut out = String::new();
    for s in specs {
        let dir = match s.direction {
            TunnelDirection::Local => "L",
            TunnelDirection::Reverse => "R",
        };
        out.push_str(&format!(
            "tunnel via=\"{}\" direction=\"{}\" local-port={} remote-host=\"{}\" remote-port={}\n",
            escape(&s.via),
            dir,
            s.local_port,
            escape(&s.remote_host),
            s.remote_port,
        ));
    }
    fs::write(&path, out)
}

pub fn upsert(inventory_path: &Path, spec: SavedTunnel) -> std::io::Result<()> {
    let mut specs = load(inventory_path);
    let key = (spec.direction, spec.via.clone(), spec.local_port);
    specs.retain(|s| (s.direction, s.via.clone(), s.local_port) != key);
    specs.push(spec);
    save(inventory_path, &specs)
}

pub fn remove(
    inventory_path: &Path,
    direction: TunnelDirection,
    via: &str,
    local_port: u16,
) -> std::io::Result<()> {
    let mut specs = load(inventory_path);
    specs.retain(|s| !(s.direction == direction && s.via == via && s.local_port == local_port));
    save(inventory_path, &specs)
}

fn entry_str(node: &kdl::KdlNode, key: &str) -> Option<String> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some(key))
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string())
}

fn entry_u16(node: &kdl::KdlNode, key: &str) -> Option<u16> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.to_string()).as_deref() == Some(key))
        .and_then(|e| e.value().as_integer())
        .and_then(|i| u16::try_from(i).ok())
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
