use glidesh::ssh::tunnel::TunnelDirection;
use std::fs;
use std::path::{Path, PathBuf};

const STORE_FILENAME: &str = ".glidesh-tunnels.kdl";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTunnel {
    pub direction: TunnelDirection,
    pub via: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    /// Bind address used by the remote sshd for `-R` forwards. Ignored for `-L`.
    pub bind_addr: String,
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
    parse(&content, &path)
}

fn parse(content: &str, source: &Path) -> Vec<SavedTunnel> {
    let doc: kdl::KdlDocument = match content.parse() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to parse {}: {}", source.display(), e);
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
        let bind_addr =
            entry_str(node, "bind-addr").unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        out.push(SavedTunnel {
            direction,
            via,
            local_port,
            remote_host,
            remote_port,
            bind_addr,
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
    fs::write(&path, render(specs))
}

fn render(specs: &[SavedTunnel]) -> String {
    let mut out = String::new();
    for s in specs {
        let dir = match s.direction {
            TunnelDirection::Local => "L",
            TunnelDirection::Reverse => "R",
        };
        match s.direction {
            TunnelDirection::Local => out.push_str(&format!(
                "tunnel via=\"{}\" direction=\"{}\" local-port={} remote-host=\"{}\" remote-port={}\n",
                escape(&s.via),
                dir,
                s.local_port,
                escape(&s.remote_host),
                s.remote_port,
            )),
            TunnelDirection::Reverse => out.push_str(&format!(
                "tunnel via=\"{}\" direction=\"{}\" local-port={} remote-host=\"{}\" remote-port={} bind-addr=\"{}\"\n",
                escape(&s.via),
                dir,
                s.local_port,
                escape(&s.remote_host),
                s.remote_port,
                escape(&s.bind_addr),
            )),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn inv(dir: &TempDir) -> PathBuf {
        dir.path().join("inventory.kdl")
    }

    fn local(via: &str, local_port: u16, remote_host: &str, remote_port: u16) -> SavedTunnel {
        SavedTunnel {
            direction: TunnelDirection::Local,
            via: via.to_string(),
            local_port,
            remote_host: remote_host.to_string(),
            remote_port,
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
        }
    }

    fn reverse(
        via: &str,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
        bind_addr: &str,
    ) -> SavedTunnel {
        SavedTunnel {
            direction: TunnelDirection::Reverse,
            via: via.to_string(),
            local_port,
            remote_host: remote_host.to_string(),
            remote_port,
            bind_addr: bind_addr.to_string(),
        }
    }

    #[test]
    fn missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load(&inv(&dir)).is_empty());
    }

    #[test]
    fn round_trip_preserves_specs() {
        let dir = TempDir::new().unwrap();
        let specs = vec![
            local("web-1", 8080, "localhost", 80),
            reverse("db-1", 5432, "127.0.0.1", 5433, "127.0.0.1"),
        ];
        save(&inv(&dir), &specs).unwrap();
        assert_eq!(load(&inv(&dir)), specs);
    }

    #[test]
    fn empty_save_removes_file() {
        let dir = TempDir::new().unwrap();
        save(&inv(&dir), &[local("web-1", 8080, "localhost", 80)]).unwrap();
        assert!(store_path(&inv(&dir)).exists());
        save(&inv(&dir), &[]).unwrap();
        assert!(!store_path(&inv(&dir)).exists());
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        let dir = TempDir::new().unwrap();
        let spec = local("host\"with\\quotes", 1, "rem\"\\", 2);
        save(&inv(&dir), std::slice::from_ref(&spec)).unwrap();
        assert_eq!(load(&inv(&dir)), vec![spec]);
    }

    #[test]
    fn invalid_kdl_returns_empty_without_panic() {
        let dir = TempDir::new().unwrap();
        std::fs::write(store_path(&inv(&dir)), "not a kdl document {{").unwrap();
        assert!(load(&inv(&dir)).is_empty());
    }

    #[test]
    fn missing_required_fields_skipped() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            store_path(&inv(&dir)),
            r#"tunnel direction="L" local-port=80 remote-port=80
tunnel via="web" direction="L" remote-host="x" remote-port=1
tunnel via="ok" direction="L" local-port=1 remote-host="x" remote-port=2
"#,
        )
        .unwrap();
        let got = load(&inv(&dir));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].via, "ok");
    }

    #[test]
    fn reverse_default_bind_addr_when_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            store_path(&inv(&dir)),
            r#"tunnel via="db" direction="R" local-port=80 remote-host="127.0.0.1" remote-port=8080
"#,
        )
        .unwrap();
        let got = load(&inv(&dir));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].bind_addr, DEFAULT_BIND_ADDR);
    }

    #[test]
    fn upsert_replaces_matching_key() {
        let dir = TempDir::new().unwrap();
        upsert(&inv(&dir), local("web-1", 8080, "old", 80)).unwrap();
        upsert(&inv(&dir), local("web-1", 8080, "new", 81)).unwrap();
        let got = load(&inv(&dir));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remote_host, "new");
        assert_eq!(got[0].remote_port, 81);
    }

    #[test]
    fn remove_drops_matching_spec() {
        let dir = TempDir::new().unwrap();
        save(
            &inv(&dir),
            &[
                local("web-1", 8080, "localhost", 80),
                local("web-2", 9090, "localhost", 80),
            ],
        )
        .unwrap();
        remove(&inv(&dir), TunnelDirection::Local, "web-1", 8080).unwrap();
        let got = load(&inv(&dir));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].via, "web-2");
    }
}
