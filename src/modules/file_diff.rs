//! Content diffs for the `file` module under `--diff`.
//!
//! The text side is pure so it can be tested without SSH; [`fetch_remote`] is the one
//! round trip, and runs only once a hash mismatch says there is something to show.

use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::secrets::SecretRegistry;
use crate::util::shell_escape;
use similar::TextDiff;

/// A file larger than this on either side gets a note instead of a diff, and a remote one
/// is not downloaded.
pub const MAX_DIFF_BYTES: u64 = 256 * 1024;

/// A `recurse=true` directory prints at most this many diff lines across all its files.
pub const MAX_DIFF_LINES: usize = 500;

/// What is on the host at the destination, as far as a diff needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    Missing,
    Content(Vec<u8>),
    TooLarge(u64),
    /// Other users on the host cannot read it, so it was not downloaded.
    Private,
    /// The size or mode could not be read, so the file was not downloaded.
    Unreadable(String),
}

/// Read the destination for a diff, checking its size and mode before downloading it.
pub async fn fetch_remote(ctx: &ModuleContext<'_>, path: &str) -> Result<Remote, GlideshError> {
    let escaped = shell_escape(path);
    // BSD stat fallback for macOS targets, as in `get_file_attrs`.
    let out = ctx
        .exec(&format!(
            "stat -c '%s %a' {escaped} 2>/dev/null || stat -f '%z %Lp' {escaped}"
        ))
        .await?;
    let parsed = out
        .stdout
        .trim()
        .split_once(' ')
        .and_then(|(size, mode)| Some((size.parse::<u64>().ok()?, others_can_read(mode)?)));
    let (size, public) = match parsed {
        Some(stat) if out.exit_code == 0 => stat,
        _ => {
            let reason = format!("{}{}", out.stdout, out.stderr);
            return Ok(Remote::Unreadable(reason.trim().to_string()));
        }
    };
    if !public {
        return Ok(Remote::Private);
    }
    if size > MAX_DIFF_BYTES {
        return Ok(Remote::TooLarge(size));
    }
    Ok(Remote::Content(ctx.download_file(path).await?))
}

/// Whether the plan's `mode` could leave the file unreadable by other users. Only an octal
/// mode can be read without the file's current mode; a symbolic one (`u=rw,go=`) goes to
/// `chmod` as written, so it counts as private.
pub fn mode_may_be_private(mode: Option<&str>) -> bool {
    mode.is_some_and(|m| others_can_read(m) != Some(true))
}

/// Whether an octal mode (`644`, `0600`, `4755`) lets other users read the file, or `None`
/// if it is not one.
pub fn others_can_read(mode: &str) -> Option<bool> {
    let others = mode.trim().chars().last()?.to_digit(8)?;
    Some(others & 4 != 0)
}

/// A unified diff from what is on the host to what the plan wants, or a one-line note
/// saying why no diff is shown. `private` is set when the plan's `mode` keeps other users
/// from reading the file.
pub fn content_diff(
    path: &str,
    remote: &Remote,
    local: &[u8],
    private: bool,
    secrets: Option<&SecretRegistry>,
) -> String {
    if let Some(note) = local_note(path, local, private) {
        return note;
    }
    let Some(new) = as_text(local) else {
        return binary(path);
    };
    let old = match remote {
        Remote::Missing => "",
        Remote::TooLarge(size) => return too_large(path, *size),
        Remote::Private => return hidden_private(path),
        Remote::Unreadable(reason) => {
            return format!("{path}: diff not shown (could not read its size and mode: {reason})");
        }
        Remote::Content(bytes) => match as_text(bytes) {
            Some(text) => text,
            None => return binary(path),
        },
    };
    // Redacting the diff is not enough: the old side can hold the value a rotated secret
    // replaced, and the `+`/`-` prefixes break a multi-line secret apart so redaction no
    // longer matches it.
    if secrets.is_some_and(|s| s.contains_secret(new) || s.contains_secret(old)) {
        return format!("{path}: diff hidden (content contains a secret)");
    }

    let from = if *remote == Remote::Missing {
        "/dev/null".to_string()
    } else {
        format!("{path} (host)")
    };
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&from, &format!("{path} (plan)"))
        .to_string()
}

/// Why no diff can be shown, judged from the plan's side alone, so the host's file is
/// never downloaded only to be discarded.
pub fn local_note(path: &str, local: &[u8], private: bool) -> Option<String> {
    // Matching registered secrets cannot catch everything: a value the plan no longer
    // uses is not registered, yet the host's copy still holds it. A file its owner keeps
    // from other users is treated as sensitive, whatever it contains.
    if private {
        return Some(hidden_private(path));
    }
    if local.len() as u64 > MAX_DIFF_BYTES {
        return Some(too_large(path, local.len() as u64));
    }
    as_text(local).is_none().then(|| binary(path))
}

/// Keep the first `max` lines, saying how many were cut.
pub fn truncate_lines(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_string();
    }
    let mut kept: String = text.lines().take(max).collect::<Vec<_>>().join("\n");
    kept.push_str(&format!("\n... {} more diff lines not shown", total - max));
    kept
}

/// Text a diff can show: valid UTF-8 without NUL bytes.
fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn hidden_private(path: &str) -> String {
    format!("{path}: diff hidden (not readable by other users)")
}

fn binary(path: &str) -> String {
    format!("{path}: binary content, diff not shown")
}

fn too_large(path: &str, size: u64) -> String {
    format!("{path}: {size} bytes, over the {MAX_DIFF_BYTES}-byte diff limit, diff not shown")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(text: &str) -> Remote {
        Remote::Content(text.as_bytes().to_vec())
    }

    #[test]
    fn a_changed_line_shows_as_removed_and_added() {
        let diff = content_diff(
            "/etc/app.conf",
            &content("a\nport=80\nc\n"),
            b"a\nport=8080\nc\n",
            false,
            None,
        );
        assert!(diff.contains("--- /etc/app.conf (host)"), "{diff}");
        assert!(diff.contains("+++ /etc/app.conf (plan)"), "{diff}");
        assert!(diff.contains("-port=80\n"), "{diff}");
        assert!(diff.contains("+port=8080\n"), "{diff}");
        assert!(diff.contains(" a\n"), "{diff}");
    }

    #[test]
    fn a_missing_file_is_diffed_against_nothing() {
        let diff = content_diff(
            "/etc/new.conf",
            &Remote::Missing,
            b"one\ntwo\n",
            false,
            None,
        );
        assert!(diff.contains("--- /dev/null"), "{diff}");
        assert!(diff.contains("+one\n+two\n"), "{diff}");
        assert!(
            !diff
                .lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---"))
        );
    }

    #[test]
    fn binary_content_on_either_side_is_not_shown() {
        let local = content_diff(
            "/bin/app",
            &content("text\n"),
            b"\x7fELF\0\x01",
            false,
            None,
        );
        assert_eq!(local, "/bin/app: binary content, diff not shown");
        let remote = content_diff(
            "/bin/app",
            &Remote::Content(vec![0xff, 0xfe]),
            b"text\n",
            false,
            None,
        );
        assert_eq!(remote, "/bin/app: binary content, diff not shown");
    }

    #[test]
    fn an_oversized_file_on_either_side_is_not_shown() {
        let big = vec![b'a'; MAX_DIFF_BYTES as usize + 1];
        let local = content_diff("/data", &Remote::Missing, &big, false, None);
        assert!(local.contains("over the 262144-byte diff limit"), "{local}");
        let remote = content_diff("/data", &Remote::TooLarge(300_000), b"small\n", false, None);
        assert!(remote.starts_with("/data: 300000 bytes"), "{remote}");
    }

    #[test]
    fn a_file_whose_size_could_not_be_read_says_why() {
        let diff = content_diff(
            "/x",
            &Remote::Unreadable("permission denied".into()),
            b"a\n",
            false,
            None,
        );
        assert_eq!(
            diff,
            "/x: diff not shown (could not read its size and mode: permission denied)"
        );
    }

    fn registry_with(secret: &str) -> SecretRegistry {
        SecretRegistry::holding(&[secret])
    }

    #[test]
    fn a_file_holding_a_secret_is_hidden_on_either_side() {
        let registry = registry_with("hunter2-password");
        let new = content_diff(
            "/etc/db",
            &content("pw=old\n"),
            b"pw=hunter2-password\n",
            false,
            Some(&registry),
        );
        assert_eq!(new, "/etc/db: diff hidden (content contains a secret)");
        let old = content_diff(
            "/etc/db",
            &content("pw=hunter2-password\n"),
            b"pw=x\n",
            false,
            Some(&registry),
        );
        assert_eq!(old, "/etc/db: diff hidden (content contains a secret)");
    }

    #[test]
    fn a_multi_line_secret_is_hidden_rather_than_split_by_the_diff() {
        let key = "-----BEGIN KEY-----\nAAAAsecretline\n-----END KEY-----";
        let registry = registry_with(key);
        let diff = content_diff(
            "/etc/key",
            &Remote::Missing,
            format!("{key}\n").as_bytes(),
            false,
            Some(&registry),
        );
        assert_eq!(diff, "/etc/key: diff hidden (content contains a secret)");
    }

    #[test]
    fn unrelated_secrets_do_not_hide_a_diff() {
        let registry = registry_with("hunter2-password");
        let diff = content_diff("/etc/app", &content("a\n"), b"b\n", false, Some(&registry));
        assert!(diff.contains("+b"), "{diff}");
    }

    #[test]
    fn long_output_is_cut_and_says_how_much() {
        let text = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            truncate_lines(&text, 3),
            "1\n2\n3\n... 7 more diff lines not shown"
        );
        assert_eq!(truncate_lines(&text, 10), text);
    }

    #[test]
    fn a_file_other_users_cannot_read_is_hidden_on_either_side() {
        let host = content_diff("/etc/app", &Remote::Private, b"a\n", false, None);
        assert_eq!(host, "/etc/app: diff hidden (not readable by other users)");
        let plan = content_diff("/etc/app", &content("a\n"), b"b\n", true, None);
        assert_eq!(plan, "/etc/app: diff hidden (not readable by other users)");
    }

    #[test]
    fn only_the_others_digit_decides_readability() {
        assert_eq!(others_can_read("644"), Some(true));
        assert_eq!(others_can_read("0604"), Some(true));
        assert_eq!(others_can_read("4755"), Some(true));
        assert_eq!(others_can_read("640"), Some(false));
        assert_eq!(others_can_read("0600"), Some(false));
        assert_eq!(others_can_read("u+rw"), None);
        assert_eq!(others_can_read(""), None);
    }

    #[test]
    fn a_symbolic_or_restrictive_plan_mode_counts_as_private() {
        assert!(!mode_may_be_private(None));
        assert!(!mode_may_be_private(Some("644")));
        assert!(mode_may_be_private(Some("0600")));
        assert!(mode_may_be_private(Some("u=rw,go=")));
        assert!(mode_may_be_private(Some("a+r")));
    }

    #[test]
    fn the_plan_side_alone_can_rule_out_a_diff() {
        assert!(local_note("/f", b"text\n", false).is_none());
        assert!(
            local_note("/f", b"text\n", true)
                .unwrap()
                .contains("not readable")
        );
        assert!(local_note("/f", b"\0", false).unwrap().contains("binary"));
        let big = vec![b'a'; MAX_DIFF_BYTES as usize + 1];
        assert!(
            local_note("/f", &big, false)
                .unwrap()
                .contains("diff limit")
        );
    }
}
