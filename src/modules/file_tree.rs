//! The paths a recursive `file` upload manages: the source tree less what `exclude` leaves
//! out, the attributes each gets, and what `prune` removes from the destination.

use crate::config::types::ParamValue;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// `exclude` patterns. One without `/` matches a name at any depth (`.git`, `*.log`); one
/// with `/` is matched against the path from the source directory (`build/**`,
/// `conf/local.kdl`). `*` and `?` match within a name, `**` any number of directories. A
/// path under an excluded directory is excluded too. Unlike `.gitignore`, there is no `!`
/// (refused, as it would read as a name), no `[…]` and no `\` escape, and a trailing `/`
/// matches files too.
#[derive(Debug, Clone, Default)]
pub struct Exclude {
    patterns: Vec<Pattern>,
}

#[derive(Debug, Clone)]
enum Pattern {
    /// Any one name on the path.
    Name(String),
    /// The path from the source directory, name by name.
    Anchored(Vec<String>),
}

impl Exclude {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let patterns = patterns
            .iter()
            .map(|raw| {
                let pattern = raw.trim_start_matches('/').trim_end_matches('/');
                if pattern.is_empty() {
                    return Err(format!("exclude pattern {raw:?} matches nothing"));
                }
                if pattern.starts_with('!') {
                    return Err(format!(
                        "exclude pattern {raw:?}: `!` does not re-include here; list only \
                         what to leave out"
                    ));
                }
                let segments: Vec<String> = pattern.split('/').map(str::to_string).collect();
                if segments
                    .iter()
                    .any(|s| s.is_empty() || s == "." || s == "..")
                {
                    return Err(format!(
                        "exclude pattern {raw:?} may not contain empty, `.` or `..` parts"
                    ));
                }
                // A leading `/` anchors a single name too, as in `.gitignore`.
                Ok(if segments.len() == 1 && !raw.starts_with('/') {
                    Pattern::Name(segments.into_iter().next().unwrap_or_default())
                } else {
                    Pattern::Anchored(segments)
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { patterns })
    }

    /// Whether `path`, relative to the source directory with `/` between names, or any
    /// directory above it is excluded.
    pub fn excludes(&self, path: &str) -> bool {
        let names: Vec<&str> = path.split('/').collect();
        self.patterns.iter().any(|pattern| match pattern {
            Pattern::Name(glob) => names.iter().any(|name| name_matches(glob, name)),
            Pattern::Anchored(segments) => {
                (1..=names.len()).any(|n| segments_match(segments, &names[..n]))
            }
        })
    }
}

/// A glob over one name: `*` any run of characters, `?` one.
fn name_matches(glob: &str, name: &str) -> bool {
    fn go(glob: &[char], name: &[char]) -> bool {
        match glob.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|skip| go(rest, &name[skip..])),
            Some(('?', rest)) => !name.is_empty() && go(rest, &name[1..]),
            Some((c, rest)) => name.first() == Some(c) && go(rest, &name[1..]),
        }
    }
    let glob: Vec<char> = glob.chars().collect();
    let name: Vec<char> = name.chars().collect();
    go(&glob, &name)
}

/// Pattern names against path names, `**` standing for any number of them, none included.
fn segments_match(segments: &[String], names: &[&str]) -> bool {
    match segments.split_first() {
        None => names.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=names.len()).any(|skip| segments_match(rest, &names[skip..]))
        }
        Some((first, rest)) => names
            .split_first()
            .is_some_and(|(name, names)| name_matches(first, name) && segments_match(rest, names)),
    }
}

/// A source directory's files and directories, relative to it with `/` between names,
/// sorted, less what `exclude` leaves out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceTree {
    pub files: Vec<String>,
    pub dirs: Vec<String>,
}

/// Walk `root`, following symlinks as reading a file does. The error names the first
/// directory that could not be read, which a recursive upload fails on.
pub fn walk(root: &Path, exclude: &Exclude) -> Result<SourceTree, String> {
    fn go(
        root: &Path,
        dir: &Path,
        exclude: &Exclude,
        tree: &mut SourceTree,
        failed: &mut Option<String>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                failed.get_or_insert_with(|| format!("{}: {e}", dir.display()));
                return;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    failed.get_or_insert_with(|| format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy();
            // Only Windows separates with `\`; elsewhere it is part of a name.
            let relative = if cfg!(windows) {
                relative.replace('\\', "/")
            } else {
                relative.into_owned()
            };
            if exclude.excludes(&relative) {
                continue;
            }
            if path.is_dir() {
                tree.dirs.push(relative);
                go(root, &path, exclude, tree, failed);
            } else {
                tree.files.push(relative);
            }
        }
    }
    let mut tree = SourceTree::default();
    let mut failed = None;
    go(root, root, exclude, &mut tree, &mut failed);
    tree.files.sort();
    tree.dirs.sort();
    match failed {
        Some(problem) => Err(problem),
        None => Ok(tree),
    }
}

/// An entry under the destination, as the host lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    Dir,
    /// A file, a symlink or anything else that is not a directory.
    Other,
}

/// What `prune` removes, relative to the destination: files (and links) first, then
/// directories deepest first, so each is empty when its turn comes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Strays {
    pub files: Vec<String>,
    pub dirs: Vec<String>,
}

impl Strays {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len() + self.dirs.len()
    }

    /// Every path, as the destination holds it: `dest/<path>`.
    pub fn paths(&self, dest: &str) -> Vec<String> {
        self.files
            .iter()
            .chain(&self.dirs)
            .map(|path| join(dest, path))
            .collect()
    }
}

/// The entries of `remote` that neither `source` has nor `exclude` leaves out. A directory
/// holding an excluded entry stays, as that entry does.
pub fn strays(remote: &[(RemoteKind, String)], source: &SourceTree, exclude: &Exclude) -> Strays {
    let kept: HashSet<&str> = source
        .files
        .iter()
        .chain(&source.dirs)
        .map(String::as_str)
        .collect();
    // Directories above an excluded entry: removing them would remove it.
    let mut holding: HashSet<&str> = HashSet::new();
    for (_, path) in remote.iter().filter(|(_, path)| exclude.excludes(path)) {
        let mut at = path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            holding.insert(parent);
            at = parent;
        }
    }
    let mut strays = Strays::default();
    let mut dirs = BTreeSet::new();
    for (kind, path) in remote {
        if kept.contains(path.as_str()) || exclude.excludes(path) {
            continue;
        }
        match kind {
            RemoteKind::Dir if !holding.contains(path.as_str()) => {
                dirs.insert(path.clone());
            }
            RemoteKind::Dir => {}
            RemoteKind::Other => strays.files.push(path.clone()),
        }
    }
    strays.files.sort();
    strays.dirs = dirs.into_iter().collect();
    strays.dirs.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then(a.cmp(b))
    });
    strays
}

/// `prune` with nothing to upload would empty the destination — a source directory left
/// empty by mistake, or an `exclude` that leaves out everything.
pub fn check_prune_source(tree: &SourceTree, src: &str, dest: &str) -> Result<(), String> {
    if tree.files.is_empty() && tree.dirs.is_empty() {
        return Err(format!(
            "prune: the source {src} has nothing to upload (empty, or all of it excluded), \
             so it would remove everything under {dest}; refusing"
        ));
    }
    Ok(())
}

/// `dest/<path>`, with `/` between.
pub fn join(dest: &str, path: &str) -> String {
    format!("{}/{}", dest.trim_end_matches('/'), path)
}

/// Parameters only a recursive upload reads.
pub const RECURSIVE_PARAMS: &[&str] = &["dir-mode", "exclude", "file-mode", "prune"];

/// A recursive upload's options, from its parameters.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub prune: bool,
    pub exclude: Exclude,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// `file-mode`, else `mode`.
    pub file_mode: Option<String>,
    /// `dir-mode`, else `mode`.
    pub dir_mode: Option<String>,
}

impl Options {
    pub fn changes_attrs(&self) -> bool {
        self.owner.is_some()
            || self.group.is_some()
            || self.file_mode.is_some()
            || self.dir_mode.is_some()
    }
}

/// A task's `exclude`, or none when it cannot be read — which [`options`] reports.
pub fn exclude_of(args: &std::collections::HashMap<String, ParamValue>) -> Exclude {
    exclude_patterns(args)
        .and_then(|patterns| Exclude::new(&patterns))
        .unwrap_or_default()
}

fn exclude_patterns(
    args: &std::collections::HashMap<String, ParamValue>,
) -> Result<Vec<String>, String> {
    match args.get("exclude") {
        None => Ok(Vec::new()),
        Some(ParamValue::List(items)) => Ok(items.clone()),
        Some(ParamValue::String(one)) => Ok(vec![one.clone()]),
        Some(_) => Err("exclude takes a list block, e.g. exclude { - \".git\" }".to_string()),
    }
}

/// Read a `file` task's recursive options, rejecting what it could not use: a recursive-only
/// parameter without `recurse=#true`, a value of the wrong kind, or a `prune` whose
/// destination is too close to `/` to be safe. `dest` is `None` while it still holds
/// `${…}`, which only a run resolves.
pub fn options(
    args: &std::collections::HashMap<String, ParamValue>,
    dest: Option<&str>,
    recurse: bool,
) -> Result<Options, String> {
    if !recurse {
        let mut given: Vec<&str> = RECURSIVE_PARAMS
            .iter()
            .copied()
            .filter(|p| args.contains_key(*p))
            .collect();
        given.sort_unstable();
        if !given.is_empty() {
            return Err(format!(
                "{} only apply with recurse=#true",
                given
                    .iter()
                    .map(|p| format!("{p}="))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let text = |key: &str| -> Result<Option<String>, String> {
        match args.get(key) {
            None => Ok(None),
            Some(ParamValue::String(s)) => Ok(Some(s.clone())),
            // `mode=644` read as a number: say how to write it.
            Some(ParamValue::Integer(n)) => {
                Err(format!("{key}= must be a quoted string: {key}=\"{n}\""))
            }
            Some(_) => Err(format!("{key}= must be a quoted string")),
        }
    };
    let prune = match args.get("prune") {
        None => false,
        Some(ParamValue::Bool(b)) => *b,
        Some(_) => return Err("prune= must be #true or #false".to_string()),
    };
    let patterns = exclude_patterns(args)?;
    if let Some(dest) = dest.filter(|_| prune) {
        check_prune_destination(dest)?;
    }
    let mode = text("mode")?;
    Ok(Options {
        prune,
        exclude: Exclude::new(&patterns)?,
        owner: text("owner")?,
        group: text("group")?,
        file_mode: text("file-mode")?.or_else(|| mode.clone()),
        dir_mode: text("dir-mode")?.or(mode),
    })
}

/// `prune` deletes: only below an absolute path of at least two names, never through `.`
/// or `..`. The host still refuses one that resolves to `/` or is a symlink.
fn check_prune_destination(dest: &str) -> Result<(), String> {
    let names: Vec<&str> = dest.split('/').filter(|n| !n.is_empty()).collect();
    if !dest.starts_with('/') || names.len() < 2 || names.iter().any(|n| *n == "." || *n == "..") {
        return Err(format!(
            "prune=#true needs an absolute destination at least two directories deep, \
             without `.` or `..` (got '{dest}')"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exclude(patterns: &[&str]) -> Exclude {
        Exclude::new(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_name_pattern_matches_at_any_depth_and_everything_under_it() {
        let ex = exclude(&[".git", "*.log"]);
        for path in [
            ".git",
            ".git/HEAD",
            "sub/.git/config",
            "app.log",
            "a/b/x.log",
        ] {
            assert!(ex.excludes(path), "{path}");
        }
        for path in ["gitignore", "a.git.bak", "log", "app.logs"] {
            assert!(!ex.excludes(path), "{path}");
        }
    }

    #[test]
    fn a_pattern_with_a_slash_is_anchored_at_the_source() {
        let ex = exclude(&["build/**", "conf/local.kdl", "/top", "**/cache"]);
        for path in [
            "build",
            "build/out/a",
            "conf/local.kdl",
            "top/x",
            "cache",
            "deep/er/cache/x",
        ] {
            assert!(ex.excludes(path), "{path}");
        }
        for path in [
            "src/build/x",
            "conf/local.kdl.bak",
            "x/conf/local.kdl",
            "sub/top",
        ] {
            assert!(!ex.excludes(path), "{path}");
        }
    }

    #[test]
    fn question_mark_and_star_stay_within_a_name() {
        let ex = exclude(&["a?c", "logs/*.txt"]);
        assert!(ex.excludes("abc") && ex.excludes("x/abc"));
        assert!(!ex.excludes("ac"));
        assert!(ex.excludes("logs/a.txt"));
        assert!(!ex.excludes("logs/sub/a.txt"));
    }

    #[test]
    fn a_pattern_that_could_not_mean_anything_is_rejected() {
        for bad in ["", "/", "a//b", "../x", "a/./b", "!keep.log"] {
            assert!(Exclude::new(&[bad.to_string()]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn walking_leaves_out_excluded_files_and_whole_directories() {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "a.conf",
            "lib/x",
            ".git/HEAD",
            "lib/debug.log",
            "empty/.keep",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("hollow")).unwrap();
        let tree = walk(dir.path(), &exclude(&[".git", "*.log"])).unwrap();
        assert_eq!(tree.files, ["a.conf", "empty/.keep", "lib/x"]);
        assert_eq!(tree.dirs, ["empty", "hollow", "lib"]);
    }

    /// With nothing to upload, prune would empty the destination.
    #[test]
    fn prune_refuses_a_source_with_nothing_to_upload() {
        let empty = SourceTree::default();
        let err = check_prune_source(&empty, "site", "/srv/site").unwrap_err();
        assert!(
            err.contains("would remove everything under /srv/site"),
            "{err}"
        );
        let one_dir = SourceTree {
            files: vec![],
            dirs: vec!["cache".into()],
        };
        assert!(check_prune_source(&one_dir, "site", "/srv/site").is_ok());
    }

    #[test]
    fn a_number_where_a_mode_belongs_says_how_to_quote_it() {
        let err = options(
            &args(&[("mode", ParamValue::Integer(644))]),
            Some("/srv/app"),
            false,
        )
        .unwrap_err();
        assert_eq!(err, "mode= must be a quoted string: mode=\"644\"");
    }

    /// `validate` reads the exclude of a task whose other options are wrong, so its template
    /// checks still skip what the run would.
    #[test]
    fn exclude_of_reads_the_patterns_alone() {
        let task = args(&[
            ("exclude", ParamValue::List(vec![".git".into()])),
            ("prune", ParamValue::String("yes".into())),
        ]);
        assert!(exclude_of(&task).excludes(".git/HEAD"));
        assert!(!exclude_of(&args(&[])).excludes(".git"));
    }

    fn remote(entries: &[(&str, bool)]) -> Vec<(RemoteKind, String)> {
        entries
            .iter()
            .map(|(path, dir)| {
                let kind = if *dir {
                    RemoteKind::Dir
                } else {
                    RemoteKind::Other
                };
                (kind, path.to_string())
            })
            .collect()
    }

    #[test]
    fn strays_are_what_the_source_lacks_less_what_is_excluded() {
        let source = SourceTree {
            files: vec!["a.conf".into(), "lib/x".into()],
            dirs: vec!["lib".into()],
        };
        let found = strays(
            &remote(&[
                ("a.conf", false),
                ("lib", true),
                ("lib/x", false),
                ("lib/old", false),
                ("gone", true),
                ("gone/deep", true),
                ("gone/deep/f", false),
                ("keep", true),
                ("keep/app.log", false),
                ("keep/stale", false),
                (".git", true),
                (".git/HEAD", false),
            ]),
            &source,
            &exclude(&[".git", "*.log"]),
        );
        assert_eq!(found.files, ["gone/deep/f", "keep/stale", "lib/old"]);
        assert_eq!(
            found.dirs,
            ["gone/deep", "gone"],
            "deepest first; keep holds a log"
        );
        assert_eq!(found.len(), 5);
        assert_eq!(
            found.paths("/srv/app/")[0],
            "/srv/app/gone/deep/f",
            "joined once, without a double slash"
        );
    }

    #[test]
    fn nothing_is_a_stray_when_the_host_matches() {
        let source = SourceTree {
            files: vec!["a".into()],
            dirs: vec![],
        };
        assert!(strays(&remote(&[("a", false)]), &source, &Exclude::default()).is_empty());
    }

    fn args(pairs: &[(&str, ParamValue)]) -> std::collections::HashMap<String, ParamValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn mode_applies_to_both_unless_a_type_has_its_own() {
        let s = |v: &str| ParamValue::String(v.to_string());
        let o = options(&args(&[("mode", s("0640"))]), Some("/srv/app"), true).unwrap();
        assert_eq!(o.file_mode.as_deref(), Some("0640"));
        assert_eq!(o.dir_mode.as_deref(), Some("0640"));
        let o = options(
            &args(&[("mode", s("0640")), ("dir-mode", s("0750"))]),
            Some("/srv/app"),
            true,
        )
        .unwrap();
        assert_eq!(o.file_mode.as_deref(), Some("0640"));
        assert_eq!(o.dir_mode.as_deref(), Some("0750"));
        let o = options(&args(&[("file-mode", s("0600"))]), Some("/srv/app"), true).unwrap();
        assert_eq!(o.file_mode.as_deref(), Some("0600"));
        assert_eq!(o.dir_mode, None);
        assert!(o.changes_attrs());
    }

    #[test]
    fn recursive_parameters_need_recurse() {
        let err = options(
            &args(&[
                ("prune", ParamValue::Bool(true)),
                ("dir-mode", ParamValue::String("0750".into())),
            ]),
            Some("/srv/app"),
            false,
        )
        .unwrap_err();
        assert_eq!(err, "dir-mode=, prune= only apply with recurse=#true");
    }

    #[test]
    fn prune_refuses_a_destination_too_close_to_the_root() {
        let prune = args(&[("prune", ParamValue::Bool(true))]);
        for dest in ["/", "/srv", "/srv/", "srv/app", "/srv/../etc", "/srv/./x"] {
            assert!(options(&prune, Some(dest), true).is_err(), "{dest}");
        }
        for dest in ["/srv/app", "/srv/app/", "/opt/a/b"] {
            assert!(options(&prune, Some(dest), true).is_ok(), "{dest}");
        }
    }

    #[test]
    fn exclude_takes_a_list_or_one_pattern() {
        let one = options(
            &args(&[("exclude", ParamValue::String(".git".into()))]),
            Some("/srv/app"),
            true,
        )
        .unwrap();
        assert!(one.exclude.excludes(".git/x"));
        let bad = options(
            &args(&[("exclude", ParamValue::Bool(true))]),
            Some("/srv/app"),
            true,
        );
        assert!(bad.unwrap_err().contains("list block"));
    }
}
