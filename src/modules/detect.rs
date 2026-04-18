use crate::error::GlideshError;
use crate::ssh::SshSession;

/// POSIX single-quote escape: wrap in `'…'`, replacing embedded `'` with `'\''`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OsInfo {
    pub id: String,
    pub version: String,
    pub family: OsFamily,
    pub pkg_manager: PkgManager,
    pub init_system: InitSystem,
    pub container_runtime: Option<ContainerRuntime>,
    pub nix_installed: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    Debian,
    RedHat,
    Arch,
    Alpine,
    Suse,
    NixOS,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PkgManager {
    Apt,
    Dnf,
    Yum,
    Pacman,
    Apk,
    Zypper,
    Nix,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitSystem {
    Systemd,
    OpenRc,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    Podman,
    Docker,
}

impl PkgManager {
    pub fn update_index_cmd(&self) -> &'static str {
        match self {
            PkgManager::Apt => "apt-get update -qq",
            PkgManager::Dnf => "dnf makecache -q",
            PkgManager::Yum => "yum makecache -q",
            PkgManager::Pacman => "pacman -Sy",
            PkgManager::Apk => "apk update -q",
            PkgManager::Zypper => "zypper refresh -q",
            PkgManager::Nix => "true",
        }
    }

    pub fn install_cmd(&self, packages: &[String]) -> String {
        let pkgs = packages.join(" ");
        match self {
            PkgManager::Apt => {
                format!("DEBIAN_FRONTEND=noninteractive apt-get install -y {}", pkgs)
            }
            PkgManager::Dnf => format!("dnf install -y {}", pkgs),
            PkgManager::Yum => format!("yum install -y {}", pkgs),
            PkgManager::Pacman => format!("pacman -S --noconfirm {}", pkgs),
            PkgManager::Apk => format!("apk add {}", pkgs),
            PkgManager::Zypper => format!("zypper install -y {}", pkgs),
            PkgManager::Nix => {
                let cmds: Vec<String> = packages
                    .iter()
                    .map(|p| format!("nix-env -iA {}", shell_escape(&format!("nixpkgs.{}", p))))
                    .collect();
                cmds.join(" && ")
            }
        }
    }

    pub fn remove_cmd(&self, packages: &[String]) -> String {
        let pkgs = packages.join(" ");
        match self {
            PkgManager::Apt => format!("DEBIAN_FRONTEND=noninteractive apt-get remove -y {}", pkgs),
            PkgManager::Dnf => format!("dnf remove -y {}", pkgs),
            PkgManager::Yum => format!("yum remove -y {}", pkgs),
            PkgManager::Pacman => format!("pacman -R --noconfirm {}", pkgs),
            PkgManager::Apk => format!("apk del {}", pkgs),
            PkgManager::Zypper => format!("zypper remove -y {}", pkgs),
            PkgManager::Nix => {
                let escaped: Vec<String> = packages.iter().map(|p| shell_escape(p)).collect();
                format!("nix-env -e {}", escaped.join(" "))
            }
        }
    }

    pub fn check_installed_cmd(&self, package: &str) -> String {
        match self {
            PkgManager::Apt => format!(
                "dpkg -s {} 2>/dev/null | grep -q 'Status: install ok installed'",
                package
            ),
            PkgManager::Dnf | PkgManager::Yum => format!("rpm -q {} >/dev/null 2>&1", package),
            PkgManager::Pacman => format!("pacman -Q {} >/dev/null 2>&1", package),
            PkgManager::Apk => format!("apk info -e {} >/dev/null 2>&1", package),
            PkgManager::Zypper => format!("rpm -q {} >/dev/null 2>&1", package),
            PkgManager::Nix => {
                let pkg_q = shell_escape(package);
                format!("nix-env -q {pkg} 2>/dev/null | grep -qw {pkg}", pkg = pkg_q)
            }
        }
    }
}

pub async fn detect_os(ssh: &SshSession) -> Result<OsInfo, GlideshError> {
    let output = ssh
        .exec("cat /etc/os-release 2>/dev/null || echo 'ID=unknown'")
        .await?;

    let mut id = String::from("unknown");
    let mut version = String::new();
    let mut id_like = String::new();

    for line in output.stdout.lines() {
        if let Some(val) = line.strip_prefix("ID=") {
            id = val.trim_matches('"').to_string();
        } else if let Some(val) = line.strip_prefix("VERSION_ID=") {
            version = val.trim_matches('"').to_string();
        } else if let Some(val) = line.strip_prefix("ID_LIKE=") {
            id_like = val.trim_matches('"').to_string();
        }
    }

    let family = detect_family(&id, &id_like);
    let pkg_manager = detect_pkg_manager(&family, &id, &version);
    let init_system = detect_init_system(&family);

    let container_runtime = detect_container_runtime(ssh).await?;
    let nix_installed = detect_nix(ssh).await?;

    Ok(OsInfo {
        id,
        version,
        family,
        pkg_manager,
        init_system,
        container_runtime,
        nix_installed,
    })
}

fn detect_family(id: &str, id_like: &str) -> OsFamily {
    let check = |s: &str| -> Option<OsFamily> {
        if s == "nixos" {
            Some(OsFamily::NixOS)
        } else if s.contains("debian") || s == "ubuntu" || s == "raspbian" || s == "linuxmint" {
            Some(OsFamily::Debian)
        } else if s.contains("rhel")
            || s.contains("fedora")
            || s == "centos"
            || s == "rocky"
            || s == "alma"
            || s == "oracle"
        {
            Some(OsFamily::RedHat)
        } else if s.contains("arch") || s == "manjaro" || s == "endeavouros" {
            Some(OsFamily::Arch)
        } else if s == "alpine" {
            Some(OsFamily::Alpine)
        } else if s.contains("suse") || s == "opensuse-leap" || s == "opensuse-tumbleweed" {
            Some(OsFamily::Suse)
        } else {
            None
        }
    };

    check(id)
        .or_else(|| check(id_like))
        .unwrap_or(OsFamily::Unknown(id.to_string()))
}

fn detect_pkg_manager(family: &OsFamily, id: &str, version: &str) -> PkgManager {
    match family {
        OsFamily::Debian => PkgManager::Apt,
        OsFamily::RedHat => {
            // CentOS < 8 uses yum
            if id == "centos" {
                if let Ok(major) = version.split('.').next().unwrap_or("0").parse::<u32>() {
                    if major < 8 {
                        return PkgManager::Yum;
                    }
                }
            }
            PkgManager::Dnf
        }
        OsFamily::Arch => PkgManager::Pacman,
        OsFamily::Alpine => PkgManager::Apk,
        OsFamily::Suse => PkgManager::Zypper,
        OsFamily::NixOS => PkgManager::Nix,
        OsFamily::Unknown(_) => PkgManager::Apt, // fallback
    }
}

fn detect_init_system(family: &OsFamily) -> InitSystem {
    match family {
        OsFamily::Alpine => InitSystem::OpenRc,
        OsFamily::Unknown(_) => InitSystem::Unknown,
        _ => InitSystem::Systemd, // NixOS, Debian, RedHat, Arch, Suse all use systemd
    }
}

async fn detect_nix(ssh: &SshSession) -> Result<bool, GlideshError> {
    // SSH non-interactive sessions don't source /etc/profile or ~/.profile, so
    // `which nix` (and `command -v nix`) can fail even when Nix is installed —
    // the Nix profile scripts are what add it to PATH. Check the common
    // absolute paths first to avoid false negatives.
    let output = ssh
        .exec(
            "[ -x /nix/var/nix/profiles/default/bin/nix ] \
             || [ -x \"$HOME/.nix-profile/bin/nix\" ] \
             || command -v nix >/dev/null 2>&1",
        )
        .await?;
    Ok(output.exit_code == 0)
}

async fn detect_container_runtime(
    ssh: &SshSession,
) -> Result<Option<ContainerRuntime>, GlideshError> {
    let podman = ssh.exec("which podman 2>/dev/null").await?;
    if podman.exit_code == 0 {
        return Ok(Some(ContainerRuntime::Podman));
    }
    let docker = ssh.exec("which docker 2>/dev/null").await?;
    if docker.exit_code == 0 {
        return Ok(Some(ContainerRuntime::Docker));
    }
    Ok(None)
}
