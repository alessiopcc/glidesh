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
    // Plugins already receive these as `@fact.*` in `vars`; keeping them off `os_info`
    // leaves the protocol v1 wire format unchanged.
    #[serde(skip)]
    pub facts: Facts,
}

/// Host facts gathered on connect, exposed to plans as `@fact.*`. A fact the host cannot
/// report (missing tool, no default route) is the empty string, never an error.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub hostname: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_count: String,
    pub mem_total_mb: String,
    pub ip_default: String,
}

const SECTION_MARKER: &str = "@@glidesh-fact ";

/// One exec for everything detection reads, so facts cost no extra round trip. POSIX sh
/// only, every probe silenced and allowed to fail: busybox hosts may lack `nproc` or `ip`.
/// `/etc/os-release` comes first, unmarked, so its parser stops at the first marker.
const PROBE_SCRIPT: &str = "cat /etc/os-release 2>/dev/null || echo 'ID=unknown'
echo '@@glidesh-fact hostname'
hostname 2>/dev/null || uname -n 2>/dev/null
echo '@@glidesh-fact kernel'
uname -r 2>/dev/null
echo '@@glidesh-fact arch'
uname -m 2>/dev/null
echo '@@glidesh-fact cpu-count'
nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null
echo '@@glidesh-fact meminfo'
grep '^MemTotal:' /proc/meminfo 2>/dev/null
echo '@@glidesh-fact route'
ip -4 route get 1.1.1.1 2>/dev/null || ip route get 1.1.1.1 2>/dev/null
true";

/// What the probe script reported, split into the `/etc/os-release` text and the facts.
struct Probe<'a> {
    os_release: &'a str,
    facts: Facts,
}

fn parse_probe(stdout: &str) -> Probe<'_> {
    let (os_release, rest) = match stdout.find(SECTION_MARKER) {
        Some(at) => stdout.split_at(at),
        None => (stdout, ""),
    };
    let mut sections: Vec<(&str, &str)> = Vec::new();
    for chunk in rest.split(SECTION_MARKER).filter(|c| !c.is_empty()) {
        let (name, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
        sections.push((name.trim(), body));
    }
    let section = |name: &str| {
        sections
            .iter()
            .find(|(n, _)| *n == name)
            .map_or("", |(_, body)| *body)
    };
    Probe {
        os_release,
        facts: Facts {
            hostname: first_line(section("hostname")),
            kernel: first_line(section("kernel")),
            arch: first_line(section("arch")),
            cpu_count: parse_cpu_count(section("cpu-count")),
            mem_total_mb: parse_mem_total_mb(section("meminfo")),
            ip_default: parse_route_src(section("route")),
        },
    }
}

fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn parse_cpu_count(body: &str) -> String {
    match first_line(body).parse::<u32>() {
        Ok(n) if n > 0 => n.to_string(),
        _ => String::new(),
    }
}

/// `MemTotal:  16318412 kB` → `15935`. The kernel always reports kB (KiB).
fn parse_mem_total_mb(body: &str) -> String {
    body.lines()
        .find_map(|l| l.trim().strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map_or(String::new(), |kb| (kb / 1024).to_string())
}

/// The address after `src` in `ip route get` output (iproute2 and busybox alike).
fn parse_route_src(body: &str) -> String {
    let mut tokens = body.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "src" {
            if let Some(addr) = tokens.next() {
                if addr.parse::<std::net::IpAddr>().is_ok() {
                    return addr.to_string();
                }
            }
        }
    }
    String::new()
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
    let output = ssh.exec(PROBE_SCRIPT).await?;
    let Probe { os_release, facts } = parse_probe(&output.stdout);

    let mut id = String::from("unknown");
    let mut version = String::new();
    let mut id_like = String::new();

    for line in os_release.lines() {
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
        facts,
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

    const UBUNTU_PROBE: &str = "PRETTY_NAME=\"Ubuntu 22.04.4 LTS\"
NAME=\"Ubuntu\"
VERSION_ID=\"22.04\"
ID=ubuntu
ID_LIKE=debian
@@glidesh-fact hostname
web-1
@@glidesh-fact kernel
5.15.0-105-generic
@@glidesh-fact arch
x86_64
@@glidesh-fact cpu-count
8
@@glidesh-fact meminfo
MemTotal:       16318412 kB
@@glidesh-fact route
1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.12 uid 0
    cache
";

    #[test]
    fn a_full_probe_yields_every_fact() {
        let probe = parse_probe(UBUNTU_PROBE);
        assert!(probe.os_release.contains("ID=ubuntu"));
        assert!(!probe.os_release.contains("@@glidesh-fact"));
        assert_eq!(
            probe.facts,
            Facts {
                hostname: "web-1".into(),
                kernel: "5.15.0-105-generic".into(),
                arch: "x86_64".into(),
                cpu_count: "8".into(),
                mem_total_mb: "15935".into(),
                ip_default: "10.0.0.12".into(),
            }
        );
    }

    /// A minimal busybox host: no `nproc`, no `ip`, no route, `getconf` answering instead.
    #[test]
    fn missing_tools_leave_facts_empty_not_failing() {
        let stdout = "ID=alpine
VERSION_ID=3.19.1
@@glidesh-fact hostname
tiny
@@glidesh-fact kernel
6.6.14-0-virt
@@glidesh-fact arch
aarch64
@@glidesh-fact cpu-count
@@glidesh-fact meminfo
@@glidesh-fact route
";
        let facts = parse_probe(stdout).facts;
        assert_eq!(facts.hostname, "tiny");
        assert_eq!(facts.arch, "aarch64");
        assert_eq!(facts.cpu_count, "");
        assert_eq!(facts.mem_total_mb, "");
        assert_eq!(facts.ip_default, "");
    }

    #[test]
    fn busybox_route_output_gives_its_source_address() {
        let body = "1.1.1.1 via 172.17.0.1 dev eth0  src 172.17.0.5\n";
        assert_eq!(parse_route_src(body), "172.17.0.5");
        assert_eq!(
            parse_route_src("RTNETLINK answers: Network is unreachable"),
            ""
        );
        assert_eq!(parse_route_src("1.1.1.1 dev eth0 src"), "");
    }

    #[test]
    fn nonsense_counts_are_dropped() {
        assert_eq!(parse_cpu_count("0\n"), "");
        assert_eq!(parse_cpu_count("nproc: not found\n"), "");
        assert_eq!(parse_cpu_count(" 4 \n"), "4");
        assert_eq!(parse_mem_total_mb("MemTotal: lots kB"), "");
        assert_eq!(parse_mem_total_mb("MemTotal:  2048 kB\n"), "2");
    }

    /// No marker at all (a shell that died after `cat`): os-release still parses and every
    /// fact is simply empty.
    #[test]
    fn output_without_markers_is_all_os_release() {
        let probe = parse_probe("ID=unknown\n");
        assert_eq!(probe.os_release, "ID=unknown\n");
        assert_eq!(probe.facts, Facts::default());
    }

    /// An `/etc/os-release` with no final newline glues the first marker onto its last line.
    #[test]
    fn a_marker_glued_to_os_release_still_splits() {
        let probe = parse_probe("ID=alpine\nVERSION_ID=3.19@@glidesh-fact arch\nx86_64\n");
        assert_eq!(probe.os_release, "ID=alpine\nVERSION_ID=3.19");
        assert_eq!(probe.facts.arch, "x86_64");
    }

    #[test]
    fn facts_stay_off_the_plugin_wire() {
        let os = OsInfo {
            id: "ubuntu".into(),
            version: "22.04".into(),
            family: OsFamily::Debian,
            pkg_manager: PkgManager::Apt,
            init_system: InitSystem::Systemd,
            container_runtime: None,
            nix_installed: false,
            facts: parse_probe(UBUNTU_PROBE).facts,
        };
        let json = serde_json::to_string(&os).unwrap();
        assert!(!json.contains("facts") && !json.contains("web-1"), "{json}");
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
