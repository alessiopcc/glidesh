//! Every plan shown in the documentation must parse, and name only parameters its modules
//! read.
//!
//! Examples are the first plans a new user copies. Five of them once used a `target` node
//! the parser has never accepted — the Getting Started plan among them — and nothing
//! caught it, because nothing ran them. The root README kept the same node after the
//! website examples were fixed, and copying it failed with `Unknown node in plan: 'target'`.
//! A `file` task with a `content` parameter no module reads went unnoticed the same way.

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

/// The ```kdl blocks of a page that are inventories: those with a top-level `jump`, `group` or
/// `host "name" "address"` node, whatever comes first (often `vars`). A block with only `vars`
/// may be a plan's `vars-file`, so it is left out.
fn inventory_blocks(page: &str) -> Vec<&str> {
    kdl_blocks(page)
        .into_iter()
        .map(|(_, block)| block)
        .filter(|block| {
            // Top-level nodes start at column 0 in every documented block.
            block
                .lines()
                .filter(|line| !line.starts_with(char::is_whitespace))
                .any(|line| {
                    is_inventory_host(line)
                        || matches!(line.split([' ', '{']).next(), Some("jump" | "group"))
                })
        })
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

/// Markdown a reader copies a plan from: the docs site, the root README, and each example's
/// README. The website test used to be the only one, so the README's `target` node survived.
fn documented_markdown() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    doc_files(&root.join("website/src/content/docs"), &mut files);
    files.push(root.join("README.md"));
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        let readme = entry.unwrap().path().join("README.md");
        if readme.is_file() {
            files.push(readme);
        }
    }
    files
}

#[test]
fn every_plan_in_the_docs_parses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = documented_markdown();

    let mut checked = 0;
    let mut readme_plans = 0;
    let mut failures = Vec::new();
    for file in &files {
        let page = std::fs::read_to_string(file).unwrap();
        for block in plan_blocks(&page) {
            checked += 1;
            if file == &root.join("README.md") {
                readme_plans += 1;
            }
            let rel = file.strip_prefix(root).unwrap_or(file);
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
    // The README is what a new user copies first. Zero plans here would hide another
    // `target` node, which is how the last one survived.
    assert!(
        readme_plans >= 1,
        "root README has no plan example; the target-node check would pass vacuously"
    );
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
    for file in documented_markdown() {
        if file.starts_with(&docs) {
            continue;
        }
        let page = std::fs::read_to_string(&file).unwrap();
        for block in inventory_blocks(&page) {
            inventories.push((file.display().to_string(), block.to_string()));
        }
    }
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

/// The docs say every example has an inventory, a plan, and a README, and both catalogs
/// link them. A directory the README omits is the next plan a reader never finds.
#[test]
fn every_example_is_listed_and_has_the_files_the_docs_promise() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    let index = std::fs::read_to_string(
        root.join("website/src/content/docs/examples/index.md"),
    )
    .unwrap();
    let mut found = 0;
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        found += 1;
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let link = format!("examples/{name}");
        if !readme.contains(&link) {
            missing.push(format!("README.md does not link {link}"));
        }
        if !index.contains(&link) {
            missing.push(format!("website examples index does not link {link}"));
        }
        for file in ["inventory.kdl", "plan.kdl", "README.md"] {
            if !dir.join(file).is_file() {
                missing.push(format!("examples/{name} has no {file}"));
            }
        }
    }
    assert!(found >= 10, "only {found} examples found");
    assert!(missing.is_empty(), "{}", missing.join("\n"));
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
