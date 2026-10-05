//! `packaging/render.sh` filling the Homebrew formula and Scoop manifest templates.
//!
//! The release workflow pushes what these produce to the tap and bucket repos, where users
//! install from them directly; a template broken here would only show at the next release.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-musl", "tar.gz"),
    ("aarch64-unknown-linux-musl", "tar.gz"),
    ("x86_64-apple-darwin", "tar.gz"),
    ("aarch64-apple-darwin", "tar.gz"),
    ("x86_64-pc-windows-msvc", "zip"),
    ("aarch64-pc-windows-msvc", "zip"),
];

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

/// A sums file for `tag` with one distinct, recognisable hash per archive.
fn sums(dir: &Path, tag: &str, targets: &[(&str, &str)]) -> PathBuf {
    let text: String = targets
        .iter()
        .enumerate()
        .map(|(i, (target, ext))| format!("{:064x}  glidesh-{}-{}.{}\n", i + 1, tag, target, ext))
        .collect();
    let path = dir.join("checksums-sha256.txt");
    std::fs::write(&path, text).unwrap();
    path
}

fn render(template: &str, tag: &str, sums: &Path) -> Output {
    Command::new("sh")
        .arg(repo("packaging/render.sh"))
        .arg(repo(template))
        .arg(tag)
        .arg(sums)
        .output()
        .unwrap()
}

fn rendered(template: &str, tag: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let out = render(template, tag, &sums(dir.path(), tag, TARGETS));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn url(tag: &str, target: &str, ext: &str) -> String {
    format!(
        "https://github.com/alessiopcc/glidesh/releases/download/{tag}/glidesh-{tag}-{target}.{ext}"
    )
}

fn hash_of(target: &str) -> String {
    let i = TARGETS.iter().position(|(t, _)| *t == target).unwrap();
    format!("{:064x}", i + 1)
}

#[test]
fn the_formula_names_each_archive_with_its_own_checksum() {
    let formula = rendered("packaging/homebrew/glidesh.rb.in", "v2.0.0");

    assert!(formula.contains("version \"2.0.0\""));
    assert!(!formula.contains('@'), "placeholder left:\n{}", formula);
    for (target, ext) in TARGETS.iter().filter(|(_, ext)| *ext == "tar.gz") {
        let url_line = format!("url \"{}\"", url("v2.0.0", target, ext));
        let sha_line = format!("sha256 \"{}\"", hash_of(target));
        let at = formula
            .find(&url_line)
            .unwrap_or_else(|| panic!("no {}", url_line));
        let next = formula[at..].lines().nth(1).unwrap().trim();
        assert_eq!(next, sha_line, "{} has the wrong checksum", target);
    }
}

#[test]
fn the_manifest_is_json_with_each_archive_and_its_checksum() {
    let manifest: serde_json::Value =
        serde_json::from_str(&rendered("packaging/scoop/glidesh.json.in", "v2.0.0")).unwrap();

    assert_eq!(manifest["version"], "2.0.0");
    assert_eq!(manifest["bin"], "glidesh.exe");
    for (arch, target) in [
        ("64bit", "x86_64-pc-windows-msvc"),
        ("arm64", "aarch64-pc-windows-msvc"),
    ] {
        let entry = &manifest["architecture"][arch];
        assert_eq!(entry["url"], url("v2.0.0", target, "zip"));
        assert_eq!(entry["hash"], hash_of(target));
    }
    // Scoop's own variables, not ours: they must reach the manifest untouched.
    assert!(
        manifest["autoupdate"]["architecture"]["64bit"]["url"]
            .as_str()
            .unwrap()
            .contains("v$version")
    );
}

#[test]
fn a_prerelease_tag_keeps_its_suffix_in_the_version() {
    let formula = rendered("packaging/homebrew/glidesh.rb.in", "v2.0.0-rc.1");

    assert!(formula.contains("version \"2.0.0-rc.1\""));
    assert!(formula.contains("/v2.0.0-rc.1/glidesh-v2.0.0-rc.1-x86_64-apple-darwin.tar.gz"));
}

#[test]
fn a_release_missing_an_archive_renders_nothing_usable() {
    let dir = tempfile::tempdir().unwrap();
    let without_arm_mac: Vec<_> = TARGETS
        .iter()
        .copied()
        .filter(|(t, _)| *t != "aarch64-apple-darwin")
        .collect();
    let sums = sums(dir.path(), "v2.0.0", &without_arm_mac);

    let out = render("packaging/homebrew/glidesh.rb.in", "v2.0.0", &sums);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no archive for aarch64-apple-darwin"));
}

#[test]
fn an_empty_sums_file_renders_nothing_usable() {
    let dir = tempfile::tempdir().unwrap();
    let sums = sums(dir.path(), "v2.0.0", &[]);

    let out = render("packaging/homebrew/glidesh.rb.in", "v2.0.0", &sums);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no archive for"));
}

#[test]
fn checksums_of_another_tag_are_not_used() {
    let dir = tempfile::tempdir().unwrap();
    let sums = sums(dir.path(), "v1.9.0", TARGETS);

    let out = render("packaging/scoop/glidesh.json.in", "v2.0.0", &sums);

    assert!(!out.status.success());
}
