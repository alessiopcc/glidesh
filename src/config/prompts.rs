//! Answers to a plan's `vars-prompt`, resolved once per run before any host is contacted.

use crate::config::types::{Plan, VarPrompt};
use crate::error::GlideshError;

/// A prompted variable's value for this run.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub name: String,
    pub value: String,
    pub secret: bool,
}

/// Every variable the plans ask for, each once: several group plans may declare the same
/// name, and one answer serves them all. A name any of them marks `secret` is treated as
/// secret, so a second declaration cannot make the answer echo or show in output.
pub fn distinct_prompts<'a>(plans: impl IntoIterator<Item = &'a Plan>) -> Vec<VarPrompt> {
    let mut prompts: Vec<VarPrompt> = Vec::new();
    for prompt in plans.into_iter().flat_map(|p| &p.prompts) {
        match prompts.iter_mut().find(|p| p.name == prompt.name) {
            Some(seen) => seen.secret |= prompt.secret,
            None => prompts.push(prompt.clone()),
        }
    }
    prompts
}

/// Split `--var name=value` flags. A malformed flag, or a name given twice, is an error.
///
/// A malformed flag is described, never echoed: it may be a password typed without its
/// `name=`, and nothing is registered for redaction yet.
fn parse_var_flags(flags: &[String]) -> Result<Vec<(String, String)>, GlideshError> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for flag in flags {
        let Some((name, value)) = flag.split_once('=').filter(|(n, _)| !n.trim().is_empty()) else {
            let problem = if flag.contains('=') {
                "a --var has no name before '='"
            } else {
                "a --var has no '='"
            };
            return Err(GlideshError::Other(format!(
                "{problem}: it must be name=value, e.g. --var release=v1.2"
            )));
        };
        let name = name.trim();
        if pairs.iter().any(|(n, _)| n == name) {
            return Err(GlideshError::Other(format!(
                "--var '{}' is given more than once",
                shown_name(name)
            )));
        }
        pairs.push((name.to_string(), value.to_string()));
    }
    Ok(pairs)
}

/// A `--var` name as an error may show it. A name made only of the characters variable names
/// use is shown, so a typo such as `relase` is visible; anything else may be the start of a
/// password that itself contains `=`, and is not.
fn shown_name(name: &str) -> &str {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain { name } else { "<not a variable name>" }
}

/// Answer each prompt: from its `--var name=value` flag, else by `ask`ing when stdin is a
/// terminal (`interactive`), else from its default. Without a terminal, every prompt left
/// with no answer is reported at once, so the run fails before connecting instead of
/// waiting on input that cannot come.
///
/// `--var` answers only declared prompts: a name no plan asks for is an error rather than a
/// general variable override, so what a run uses stays visible in the plan.
pub fn resolve_answers(
    prompts: &[VarPrompt],
    var_flags: &[String],
    interactive: bool,
    mut ask: impl FnMut(&VarPrompt) -> Result<String, GlideshError>,
) -> Result<Vec<Answer>, GlideshError> {
    let given = parse_var_flags(var_flags)?;
    let unknown: Vec<&str> = given
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !prompts.iter().any(|p| p.name == *n))
        .map(shown_name)
        .collect();
    if !unknown.is_empty() {
        let declared = if prompts.is_empty() {
            "the plan declares no vars-prompt".to_string()
        } else {
            format!(
                "its vars-prompt declares: {}",
                prompts
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(GlideshError::Other(format!(
            "--var names a variable the plan does not ask for: {} ({declared}). --var only \
             answers a vars-prompt",
            unknown.join(", ")
        )));
    }
    let given_value = |name: &str| {
        given
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };

    if !interactive {
        let missing: Vec<&str> = prompts
            .iter()
            .filter(|p| p.default.is_none() && given_value(&p.name).is_none())
            .map(|p| p.name.as_str())
            .collect();
        if !missing.is_empty() {
            let fix: Vec<String> = missing
                .iter()
                .map(|n| format!("--var {n}=<value>"))
                .collect();
            return Err(GlideshError::Other(format!(
                "stdin is not a terminal, so these prompted variables need an answer on the \
                 command line: {}. Pass {}",
                missing.join(", "),
                fix.join(" ")
            )));
        }
    }

    prompts
        .iter()
        .map(|p| {
            let value = match given_value(&p.name) {
                Some(v) => v,
                None if interactive => ask(p)?,
                None => p.default.clone().unwrap_or_default(),
            };
            Ok(Answer {
                name: p.name.clone(),
                value,
                secret: p.secret,
            })
        })
        .collect()
}

/// Put each answer into the plan variables of the plans that asked for it — the same slot
/// as plan `vars`, which `parse_plan` keeps from also naming a prompted variable.
pub fn apply_answers(plan: &mut Plan, answers: &[Answer]) {
    for answer in answers {
        if plan.prompts.iter().any(|p| p.name == answer.name) {
            plan.vars.insert(answer.name.clone(), answer.value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_plan;

    fn prompt(name: &str, default: Option<&str>, secret: bool) -> VarPrompt {
        VarPrompt {
            name: name.to_string(),
            text: format!("{name}?"),
            default: default.map(str::to_string),
            secret,
        }
    }

    fn flags(f: &[&str]) -> Vec<String> {
        f.iter().map(|s| s.to_string()).collect()
    }

    fn never_ask(p: &VarPrompt) -> Result<String, GlideshError> {
        panic!("asked for {} without a terminal", p.name)
    }

    #[test]
    fn a_var_flag_answers_its_prompt() {
        let prompts = [prompt("release", Some("main"), false)];
        let answers =
            resolve_answers(&prompts, &flags(&["release=v1.2=rc"]), false, never_ask).unwrap();
        assert_eq!(
            answers,
            [Answer {
                name: "release".into(),
                value: "v1.2=rc".into(),
                secret: false
            }]
        );
    }

    #[test]
    fn without_a_terminal_the_default_is_taken() {
        let prompts = [prompt("release", Some("main"), false)];
        let answers = resolve_answers(&prompts, &[], false, never_ask).unwrap();
        assert_eq!(answers[0].value, "main");
    }

    #[test]
    fn without_a_terminal_every_missing_answer_is_named() {
        let prompts = [
            prompt("release", None, false),
            prompt("region", Some("eu"), false),
            prompt("db-password", None, true),
        ];
        let err = resolve_answers(&prompts, &[], false, never_ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("release, db-password"), "{err}");
        assert!(
            err.contains("--var release=<value> --var db-password=<value>"),
            "{err}"
        );
        assert!(!err.contains("region"), "{err}");
    }

    #[test]
    fn a_terminal_asks_only_what_the_command_line_left_open() {
        let prompts = [prompt("release", None, false), prompt("tag", None, true)];
        let mut asked = Vec::new();
        let answers = resolve_answers(&prompts, &flags(&["release=v2"]), true, |p| {
            asked.push(p.name.clone());
            Ok("typed".to_string())
        })
        .unwrap();
        assert_eq!(asked, ["tag"]);
        assert_eq!(answers[0].value, "v2");
        assert_eq!(answers[1].value, "typed");
        assert!(answers[1].secret);
    }

    #[test]
    fn a_var_flag_for_an_undeclared_name_is_an_error() {
        let prompts = [prompt("release", None, false)];
        let err = resolve_answers(&prompts, &flags(&["relase=v1"]), false, never_ask)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("relase") && err.contains("declares: release"),
            "{err}"
        );

        let err = resolve_answers(&[], &flags(&["x=1"]), false, never_ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("declares no vars-prompt"), "{err}");
    }

    /// A password containing `=` passed without its `name=` splits into a "name" that is the
    /// start of the password: only a name that looks like one is echoed.
    #[test]
    fn an_undeclared_name_is_shown_only_when_it_looks_like_a_name() {
        let prompts = [prompt("db-password", None, true)];
        for (flag, shown) in [("relase=v1", true), ("hunter2!x=rest", false)] {
            let err = resolve_answers(&prompts, &flags(&[flag]), false, never_ask)
                .unwrap_err()
                .to_string();
            let name = flag.split_once('=').unwrap().0;
            assert_eq!(err.contains(name), shown, "{flag}: {err}");
            assert!(err.contains("does not ask for"), "{err}");
        }
    }

    #[test]
    fn a_malformed_or_repeated_var_flag_is_an_error() {
        let prompts = [prompt("release", None, false)];
        for bad in [&["release"][..], &["=v1"], &["release=a", "release=b"]] {
            assert!(
                resolve_answers(&prompts, &flags(bad), false, never_ask).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    /// A password typed without its `name=` must not end up on stderr before anything is
    /// registered to mask it.
    #[test]
    fn a_malformed_var_flag_is_not_echoed() {
        let prompts = [prompt("db-password", None, true)];
        for bad in ["hunter2-secret", "=hunter2-secret"] {
            let err = resolve_answers(&prompts, &flags(&[bad]), false, never_ask)
                .unwrap_err()
                .to_string();
            assert!(err.contains("name=value"), "{err}");
            assert!(!err.contains("hunter2"), "{bad}: {err}");
        }
    }

    #[test]
    fn plans_sharing_a_prompt_ask_it_once_and_secret_wins() {
        let a = parse_plan(
            "plan \"a\" {\n vars-prompt {\n pass \"Password\"\n release \"Release\"\n }\n}",
        )
        .unwrap();
        let b =
            parse_plan("plan \"b\" {\n vars-prompt {\n pass \"Pass\" secret=#true\n }\n}").unwrap();
        let prompts = distinct_prompts([&a, &b]);
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].name == "pass" && prompts[0].secret);
        assert_eq!(prompts[0].text, "Password");
    }

    #[test]
    fn answers_reach_only_the_plans_that_asked() {
        let mut asking =
            parse_plan("plan \"a\" {\n vars-prompt {\n release \"Release\"\n }\n}").unwrap();
        let mut other = parse_plan("plan \"b\" { }").unwrap();
        let answers = [Answer {
            name: "release".into(),
            value: "v3".into(),
            secret: false,
        }];
        apply_answers(&mut asking, &answers);
        apply_answers(&mut other, &answers);
        assert_eq!(asking.vars["release"], "v3");
        assert!(other.vars.is_empty());
    }
}
