//! Every plan shown in the documentation must parse, and name only parameters its modules
//! read.
//!
//! Examples are the first plans a new user copies. Five of them once used a `target` node
//! the parser has never accepted — the Getting Started plan among them — and nothing
//! caught it, because nothing ran them. A `file` task with a `content` parameter no module
//! reads went unnoticed the same way.

use glidesh::config::types::Plan;
use glidesh::modules::ModuleRegistry;
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

/// Nodes a plan fragment can start with: a task of one of these modules.
const TASKS: &[&str] = &[
    "container",
    "disk",
    "external",
    "file",
    "host",
    "nix",
    "package",
    "shell",
    "systemd",
    "user",
];

/// A page's ```kdl blocks, each with its first line that is not blank or a comment.
fn kdl_blocks(page: &str) -> Vec<(&str, &str)> {
    let mut blocks = Vec::new();
    let mut rest = page;
    while let Some(start) = rest.find("```kdl") {
        let body = &rest[start + "```kdl".len()..];
        let Some(end) = body.find("```") else { break };
        let block = &body[..end];
        let first_line = block
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("//"))
            .unwrap_or_default();
        blocks.push((first_line, block));
        rest = &body[end + 3..];
    }
    blocks
}

/// An inventory's `host "name" "address"`, which is not a `host` task.
fn is_inventory_host(first_line: &str) -> bool {
    first_line
        .strip_prefix("host \"")
        .and_then(|rest| rest.split_once('"'))
        .is_some_and(|(_, after)| after.trim_start().starts_with('"'))
}

/// The ```kdl blocks of a page that are plans, each as a whole plan: a block whose first node
/// is `plan` as it is, one that starts with a `step` or a task wrapped in a plan (and a step).
/// Inventories, `vars` files and other fragments are left out.
fn plan_blocks(page: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    for (first_line, block) in kdl_blocks(page) {
        match first_line.split([' ', '{']).next() {
            _ if is_inventory_host(first_line) => {}
            Some("plan") => blocks.push(block.to_string()),
            Some("step") => blocks.push(format!("plan \"doc\" {{\n{block}\n}}")),
            Some(node) if TASKS.contains(&node) => {
                blocks.push(format!("plan \"doc\" {{ step \"doc\" {{\n{block}\n}} }}"))
            }
            _ => {}
        }
    }
    blocks
}

/// The ```kdl blocks of a page that are inventories: those starting with a top-level `jump`,
/// a `group` or a `host "name" "address"`. One starting with `vars` may be a plan's
/// `vars-file`, so it is left out.
fn inventory_blocks(page: &str) -> Vec<&str> {
    kdl_blocks(page)
        .into_iter()
        .filter(|(first_line, _)| {
            is_inventory_host(first_line)
                || matches!(first_line.split([' ', '{']).next(), Some("jump" | "group"))
        })
        .map(|(_, block)| block)
        .collect()
}

/// What `run` and `validate` reject in `plan`: a task parameter its module does not read, or
/// a module that does not exist. A plugin is not installed here, so it is never missing.
fn module_problems(plan: &Plan) -> Vec<String> {
    let registry = ModuleRegistry::new();
    let mut problems = registry.plan_problems(plan);
    let builtin = |module: &str| {
        module.starts_with("external.")
            || module == "host"
            || registry.builtin_names().any(|name| name == module)
    };
    let all_known = plan
        .steps()
        .iter()
        .flat_map(|step| step.all_tasks())
        .all(|task| builtin(&task.module));
    if all_known {
        problems.retain(|p| !p.starts_with("Unknown module(s)"));
    }
    problems
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
            let rel = file.strip_prefix(&docs).unwrap_or(file);
            match glidesh::config::parse_plan(&block) {
                Ok(plan) => failures.extend(
                    module_problems(&plan)
                        .into_iter()
                        .map(|p| format!("{}: {p}", rel.display())),
                ),
                Err(e) => failures.push(format!("{}: {e}", rel.display())),
            }
        }
    }

    // Guards the extraction itself: a change that found no plans would pass vacuously.
    assert!(checked >= 100, "only {checked} plan examples found");
    assert!(
        failures.is_empty(),
        "{} of {checked} documented plans do not parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every inventory the docs show and every `examples/*/inventory*.kdl` parses.
#[test]
fn every_inventory_in_the_docs_and_examples_parses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs = root.join("website/src/content/docs");
    let mut files = Vec::new();
    doc_files(&docs, &mut files);
    let mut inventories: Vec<(String, String)> = Vec::new();
    for file in &files {
        let page = std::fs::read_to_string(file).unwrap();
        let rel = file
            .strip_prefix(&docs)
            .unwrap_or(file)
            .display()
            .to_string();
        for block in inventory_blocks(&page) {
            inventories.push((rel.clone(), block.to_string()));
        }
    }
    let docs_count = inventories.len();
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        for file in std::fs::read_dir(entry.unwrap().path())
            .into_iter()
            .flatten()
        {
            let path = file.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with("inventory") && name.ends_with(".kdl") {
                let content = std::fs::read_to_string(&path).unwrap();
                inventories.push((path.display().to_string(), content));
            }
        }
    }

    // Guards the extraction itself: a change that found none would pass vacuously.
    assert!(
        docs_count >= 10,
        "only {docs_count} inventories found in the docs"
    );
    assert!(
        inventories.len() >= docs_count + 10,
        "only {} example inventories found",
        inventories.len() - docs_count
    );
    let failures: Vec<String> = inventories
        .iter()
        .filter_map(|(source, content)| {
            glidesh::config::parse_inventory(content)
                .err()
                .map(|e| format!("{source}: {e}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
                    .chain(module_problems(&plan))
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

/// Every `--tags` / `--skip-tags` an example's README shows must name tags its plan uses, or
/// the command it tells readers to copy fails.
#[test]
fn every_tag_an_example_readme_uses_exists_in_its_plan() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let dir = entry.unwrap().path();
        let Ok(readme) = std::fs::read_to_string(dir.join("README.md")) else {
            continue;
        };
        for line in readme.lines().filter(|l| l.contains("glidesh run")) {
            let words: Vec<&str> = line.split_whitespace().collect();
            let value = |flag: &str| {
                words
                    .windows(2)
                    .find(|w| w[0] == flag)
                    .map(|w| w[1].to_string())
            };
            let (tags, skip) = (value("--tags"), value("--skip-tags"));
            if tags.is_none() && skip.is_none() {
                continue;
            }
            let content = std::fs::read_to_string(dir.join("plan.kdl")).unwrap();
            let mut plan = glidesh::config::parse_plan(&content).unwrap();
            glidesh::config::resolve_includes(&mut plan, &dir).unwrap();
            glidesh::config::tags::TagFilter::from_args(tags.as_deref(), skip.as_deref())
                .and_then(|f| f.check_known([&plan].into_iter()))
                .unwrap_or_else(|e| panic!("{}: `{line}`: {e}", dir.display()));
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no example shows --tags; the check would pass vacuously"
    );
}
