use crate::error::GlideshError;
use crate::ssh::SshSession;
use crate::util::shell_escape;

// Nix commands live in profile-script paths that non-login SSH shells don't
// pick up. `PkgManager::Nix` prepends this so `nix-env` is resolvable.
const NIX_PATH_PREFIX: &str = "export PATH=/nix/var/nix/profiles/default/bin:$HOME/.nix-profile/bin:/run/current-system/sw/bin:$PATH";

#[derive(Debug, Clone, serde::Serialize)]
pub struct OsInfo {
    pub id: String,
    pub version: String,
    pub family: OsFamily,
    pub pkg_manager: PkgManager,
    pub init_system: InitSystem,
    pub container_runtime: Option<ContainerRuntime>,
    // Skip when false so the external-plugin JSON wire format is unchanged for
    // non-NixOS hosts — only hosts with Nix present emit this new field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nix_installed: bool,
}

// The explicit renames, here and on `InitSystem`, keep the plugin wire on the same vocabulary
// as the `@os.*` vars: `rename_all` alone emits `red_hat`, `nix_o_s` and `open_rc`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    Debian,
    #[serde(rename = "redhat")]
    RedHat,
    Arch,
    Alpine,
    Suse,
    #[serde(rename = "nixos")]
    NixOS,
    Unknown(String),
}

impl OsFamily {
    /// The family as a plan sees it in `${@os.family}`. An unrecognised OS reports its raw
    /// `/etc/os-release` `ID`, so a plan can still branch on it.
    pub fn as_str(&self) -> &str {
        match self {
            OsFamily::Debian => "debian",
            OsFamily::RedHat => "redhat",
            OsFamily::Arch => "arch",
            OsFamily::Alpine => "alpine",
            OsFamily::Suse => "suse",
            OsFamily::NixOS => "nixos",
            OsFamily::Unknown(id) => id,
        }
    }
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
    #[serde(rename = "openrc")]
    OpenRc,
    Unknown,
}

impl InitSystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            InitSystem::Systemd => "systemd",
            InitSystem::OpenRc => "openrc",
            InitSystem::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    Podman,
    Docker,
}

impl ContainerRuntime {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerRuntime::Podman => "podman",
            ContainerRuntime::Docker => "docker",
        }
    }
}

impl PkgManager {
    pub fn as_str(&self) -> &'static str {
        match self {
            PkgManager::Apt => "apt",
            PkgManager::Dnf => "dnf",
            PkgManager::Yum => "yum",
            PkgManager::Pacman => "pacman",
            PkgManager::Apk => "apk",
            PkgManager::Zypper => "zypper",
            PkgManager::Nix => "nix",
        }
    }

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
                format!("{}; {}", NIX_PATH_PREFIX, cmds.join(" && "))
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
                format!("{}; nix-env -e {}", NIX_PATH_PREFIX, escaped.join(" "))
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
                format!(
                    "{}; nix-env -q {pkg} 2>/dev/null | grep -qw {pkg}",
                    NIX_PATH_PREFIX,
                    pkg = pkg_q
                )
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
    // Nix's PATH entry comes from /etc/profile, which SSH non-interactive
    // sessions don't source — so `command -v nix` alone gives false negatives.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A plugin reads these values off the wire and a plan reads them from `${@os.*}`; if
    /// the two ever disagree, a plugin and a plan branching on the same host see different
    /// answers.
    #[test]
    fn plugins_and_plans_share_one_vocabulary() {
        for family in [
            OsFamily::Debian,
            OsFamily::RedHat,
            OsFamily::Arch,
            OsFamily::Alpine,
            OsFamily::Suse,
            OsFamily::NixOS,
        ] {
            assert_eq!(wire(&family), family.as_str());
        }
        for pm in [
            PkgManager::Apt,
            PkgManager::Dnf,
            PkgManager::Yum,
            PkgManager::Pacman,
            PkgManager::Apk,
            PkgManager::Zypper,
            PkgManager::Nix,
        ] {
            assert_eq!(wire(&pm), pm.as_str());
        }
        for init in [InitSystem::Systemd, InitSystem::OpenRc, InitSystem::Unknown] {
            assert_eq!(wire(&init), init.as_str());
        }
        for rt in [ContainerRuntime::Podman, ContainerRuntime::Docker] {
            assert_eq!(wire(&rt), rt.as_str());
        }
    }

    #[test]
    fn multi_word_names_are_typeable() {
        assert_eq!(OsFamily::RedHat.as_str(), "redhat");
        assert_eq!(OsFamily::NixOS.as_str(), "nixos");
        assert_eq!(InitSystem::OpenRc.as_str(), "openrc");
    }

    #[test]
    fn an_unrecognised_family_reports_its_raw_id() {
        let family = OsFamily::Unknown("plan9".to_string());
        assert_eq!(family.as_str(), "plan9");
        assert_eq!(
            serde_json::to_value(&family).unwrap(),
            serde_json::json!({"unknown": "plan9"})
        );
    }
}
