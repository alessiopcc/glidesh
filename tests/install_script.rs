//! `website/public/install.sh` against a fake release on disk (`GLIDESH_BASE_URL=file://…`).
//!
//! The script is what most Linux and macOS users run first, piped from curl, and it decides
//! which binary lands on their machine. These tests pin that it picks the right archive,
//! refuses one whose checksum differs, and installs nothing when it fails.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("website/public/install.sh")
}

/// A fake release `tag` under `root`, laid out like GitHub's: one archive per target in
/// `targets`, each holding a `glidesh` that prints its version and target, and the sums
/// file. With `latest`, the sums file is also served as the latest release's.
fn release(root: &Path, tag: &str, targets: &[&str], latest: bool) {
    let dir = root.join("download").join(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let mut sums = String::new();
    for target in targets {
        let stage = tempfile::tempdir().unwrap();
        let bin = stage.path().join("glidesh");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"glidesh {} {}\"\n", &tag[1..], target),
        )
        .unwrap();
        let name = format!("glidesh-{}-{}.tar.gz", tag, target);
        let status = Command::new("tar")
            .arg("-czf")
            .arg(dir.join(&name))
            .arg("-C")
            .arg(stage.path())
            .arg("glidesh")
            .status()
            .unwrap();
        assert!(status.success());
        sums.push_str(&format!("{}  {}\n", sha256(&dir.join(&name)), name));
    }
    std::fs::write(dir.join("checksums-sha256.txt"), &sums).unwrap();
    if latest {
        let latest = root.join("latest/download");
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(latest.join("checksums-sha256.txt"), &sums).unwrap();
    }
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg("sha256sum \"$1\" 2>/dev/null || shasum -a 256 \"$1\"")
        .arg("sh")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_string()
}

fn install(root: &Path, dest: &Path, version: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg(script())
        .args(args)
        .env("GLIDESH_BASE_URL", format!("file://{}", root.display()))
        .env("GLIDESH_INSTALL_DIR", dest)
        .env_remove("GLIDESH_VERSION");
    if let Some(version) = version {
        cmd.env("GLIDESH_VERSION", version);
    }
    cmd.output().unwrap()
}

fn installed(dest: &Path) -> String {
    let out = Command::new(dest.join("glidesh")).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The target this machine should get from a release that ships every archive.
fn expected_target() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => panic!("no glidesh build for {}", other),
    };
    if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", arch)
    } else {
        format!("{}-unknown-linux-musl", arch)
    }
}

#[test]
fn the_latest_release_is_installed_for_this_machine() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    release(root.path(), "v8.0.0", TARGETS, false);
    release(root.path(), "v9.0.0", TARGETS, true);

    let out = install(root.path(), dest.path(), None, &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        installed(dest.path()),
        format!("glidesh 9.0.0 {}", expected_target())
    );
}

#[test]
fn a_pinned_version_is_installed_instead_of_the_latest() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    release(root.path(), "v8.0.0", TARGETS, false);
    release(root.path(), "v9.0.0", TARGETS, true);

    let out = install(root.path(), dest.path(), Some("v8.0.0"), &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(installed(dest.path()).starts_with("glidesh 8.0.0 "));
}

#[test]
fn running_it_again_upgrades_the_installed_binary() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    release(root.path(), "v8.0.0", TARGETS, false);
    release(root.path(), "v9.0.0", TARGETS, true);

    assert!(
        install(root.path(), dest.path(), Some("v8.0.0"), &[])
            .status
            .success()
    );
    let out = install(root.path(), dest.path(), None, &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(installed(dest.path()).starts_with("glidesh 9.0.0 "));
}

#[cfg(target_os = "linux")]
#[test]
fn a_release_without_a_musl_build_installs_the_glibc_one() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let gnu: Vec<&str> = TARGETS
        .iter()
        .copied()
        .filter(|t| !t.ends_with("-musl"))
        .collect();
    release(root.path(), "v1.2.0", &gnu, true);

    let out = install(root.path(), dest.path(), None, &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(installed(dest.path()).ends_with("-unknown-linux-gnu"));
}

#[test]
fn an_archive_whose_checksum_differs_is_not_installed() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    release(root.path(), "v9.0.0", TARGETS, true);
    let name = format!("glidesh-v9.0.0-{}.tar.gz", expected_target());
    let archive = root.path().join("download/v9.0.0").join(&name);
    let mut bytes = std::fs::read(&archive).unwrap();
    bytes.extend_from_slice(b"tampered");
    std::fs::write(&archive, bytes).unwrap();

    let out = install(root.path(), dest.path(), None, &[]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("checksum mismatch"),
        "{}",
        stderr(&out)
    );
    assert!(!dest.path().join("glidesh").exists());
}

#[test]
fn a_release_without_this_machines_build_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let others: Vec<&str> = TARGETS
        .iter()
        .copied()
        .filter(|t| !t.starts_with(std::env::consts::ARCH))
        .collect();
    release(root.path(), "v9.0.0", &others, true);

    let out = install(root.path(), dest.path(), None, &[]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("has no archive"), "{}", stderr(&out));
    assert!(!dest.path().join("glidesh").exists());
}

#[test]
fn a_version_that_does_not_exist_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    release(root.path(), "v9.0.0", TARGETS, true);

    let out = install(root.path(), dest.path(), Some("v7.7.7"), &[]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("could not download"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn help_names_every_option_and_an_unknown_argument_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();

    let help = install(root.path(), dest.path(), None, &["--help"]);
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(help.status.success());
    for needle in [
        "GLIDESH_VERSION",
        "GLIDESH_INSTALL_DIR",
        "GLIDESH_BASE_URL",
        "scoop",
    ] {
        assert!(text.contains(needle), "--help lacks {}", needle);
    }

    let bad = install(root.path(), dest.path(), None, &["--version"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("unknown argument"));
}
