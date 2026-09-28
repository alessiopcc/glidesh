//! Every plan shown in the documentation must parse.
//!
//! Examples are the first plans a new user copies. Five of them once used a `target` node
//! the parser has never accepted — the Getting Started plan among them — and nothing
//! caught it, because nothing ran them.

use std::path::{Path, PathBuf};

fn doc_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            doc_files(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md" | "mdx")
        ) {
            out.push(path);
        }
    }
}

/// The ```kdl blocks of a page that are whole plans. Inventories, `vars` files and
/// fragments are other blocks; only a block whose first node is `plan` is a plan.
fn plan_blocks(page: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = page;
    while let Some(start) = rest.find("```kdl") {
        let body = &rest[start + "```kdl".len()..];
        let Some(end) = body.find("```") else { break };
        let block = &body[..end];
        let first = block
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("//"));
        if first.is_some_and(|l| l.starts_with("plan \"")) {
            blocks.push(block.to_string());
        }
        rest = &body[end + 3..];
    }
    blocks
}

#[test]
fn every_plan_in_the_docs_parses() {
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("website/src/content/docs");
    let mut files = Vec::new();
    doc_files(&docs, &mut files);

    let mut checked = 0;
    let mut failures = Vec::new();
    for file in &files {
        let page = std::fs::read_to_string(file).unwrap();
        for block in plan_blocks(&page) {
            checked += 1;
            if let Err(e) = glidesh::config::parse_plan(&block) {
                let rel = file.strip_prefix(&docs).unwrap_or(file);
                failures.push(format!("{}: {e}", rel.display()));
            }
        }
    }

    // Guards the extraction itself: a change that found no plans would pass vacuously.
    assert!(checked >= 20, "only {checked} plan examples found");
    assert!(
        failures.is_empty(),
        "{} of {checked} documented plans do not parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every plan under `examples/` loads as `run` loads it — includes and `vars-file`s resolved
/// — and every local `file` source it names exists.
#[test]
fn every_example_plan_resolves() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(&examples).unwrap() {
        let plan_path = entry.unwrap().path().join("plan.kdl");
        if !plan_path.exists() {
            continue;
        }
        checked += 1;
        let content = std::fs::read_to_string(&plan_path).unwrap();
        let dir = plan_path.parent().unwrap();
        let result = glidesh::config::parse_plan(&content).and_then(|mut plan| {
            glidesh::config::resolve_includes(&mut plan, dir)?;
            Ok(plan)
        });
        match result {
            Ok(plan) => failures.extend(
                glidesh::config::checks::missing_file_sources(&plan, dir)
                    .into_iter()
                    .map(|m| format!("{}: {m}", plan_path.display())),
            ),
            Err(e) => failures.push(format!("{}: {e}", plan_path.display())),
        }
    }
    assert!(checked >= 10, "only {checked} example plans found");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An included plan's variables reach the run: the multi-tier example's `${ntp-service}` is
/// defined only in the plan it includes.
#[test]
fn the_multi_tier_example_gets_its_variable_from_the_included_plan() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/multi-tier");
    let content = std::fs::read_to_string(dir.join("plan.kdl")).unwrap();
    let mut plan = glidesh::config::parse_plan(&content).unwrap();
    glidesh::config::resolve_includes(&mut plan, &dir).unwrap();
    assert_eq!(plan.vars["ntp-service"], "chrony");
}
