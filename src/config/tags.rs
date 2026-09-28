//! Step tags and the `--tags` / `--skip-tags` selection of a run.

use crate::config::types::Plan;
use crate::error::GlideshError;

/// A step tagged `always` runs even when `--tags` does not name it, so steps that register
/// variables for later ones can keep a partial run working. `--skip-tags always` still
/// skips it.
pub const ALWAYS: &str = "always";

/// Parse a comma-separated tag list, as written in `tags=` or on the command line.
pub fn parse_list(list: &str, what: &str) -> Result<Vec<String>, GlideshError> {
    let mut tags = Vec::new();
    for tag in list.split(',').map(str::trim) {
        if tag.is_empty() || tag.contains(char::is_whitespace) {
            return Err(GlideshError::ConfigParse {
                message: format!(
                    "{what}: invalid tag list '{list}' (expected names separated by commas, \
                     like \"web,deploy\")"
                ),
            });
        }
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    Ok(tags)
}

/// Which steps a run selects. Empty selects every step.
#[derive(Debug, Clone, Default)]
pub struct TagFilter {
    /// `--tags`: only steps carrying one of these (or `always`) run.
    pub only: Vec<String>,
    /// `--skip-tags`: steps carrying any of these never run; wins over `only`.
    pub skip: Vec<String>,
}

impl TagFilter {
    pub fn from_args(tags: Option<&str>, skip_tags: Option<&str>) -> Result<Self, GlideshError> {
        Ok(Self {
            only: tags
                .map(|t| parse_list(t, "--tags"))
                .transpose()?
                .unwrap_or_default(),
            skip: skip_tags
                .map(|t| parse_list(t, "--skip-tags"))
                .transpose()?
                .unwrap_or_default(),
        })
    }

    /// Why a step with these tags is left out of the run, or `None` when it runs.
    pub fn excludes(&self, step_tags: &[String]) -> Option<String> {
        if let Some(tag) = step_tags.iter().find(|t| self.skip.contains(t)) {
            return Some(format!("--skip-tags {tag}"));
        }
        let selected = self.only.is_empty()
            || step_tags
                .iter()
                .any(|t| t == ALWAYS || self.only.contains(t));
        (!selected).then(|| format!("not in --tags {}", self.only.join(",")))
    }

    /// Fail on a tag no step carries. A typo in `--tags` would silently run nothing, and one
    /// in `--skip-tags` would run the very steps it was meant to hold back.
    pub fn check_known<'a>(
        &self,
        plans: impl Iterator<Item = &'a Plan>,
    ) -> Result<(), GlideshError> {
        let mut known: Vec<&str> = Vec::new();
        for plan in plans {
            for step in plan.steps() {
                known.extend(step.tags.iter().map(String::as_str));
            }
        }
        let unknown: Vec<&str> = self
            .only
            .iter()
            .chain(&self.skip)
            .map(String::as_str)
            .filter(|t| !known.contains(t))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        known.sort_unstable();
        known.dedup();
        let known = if known.is_empty() {
            "no step has tags".to_string()
        } else {
            format!("tags in use: {}", known.join(", "))
        };
        Err(GlideshError::Other(format!(
            "no step is tagged {} ({known})",
            unknown
                .iter()
                .map(|t| format!("'{t}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    fn filter(only: Option<&str>, skip: Option<&str>) -> TagFilter {
        TagFilter::from_args(only, skip).unwrap()
    }

    #[test]
    fn a_list_is_trimmed_and_deduplicated() {
        assert_eq!(
            parse_list(" web, db ,web", "x").unwrap(),
            tags(&["web", "db"])
        );
    }

    #[test]
    fn an_empty_or_spaced_tag_is_rejected() {
        for bad in ["", "web,", ",web", "web,,db", "my tag"] {
            assert!(parse_list(bad, "x").is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn no_filter_runs_everything() {
        let f = TagFilter::default();
        assert_eq!(f.excludes(&[]), None);
        assert_eq!(f.excludes(&tags(&["web"])), None);
    }

    #[test]
    fn tags_select_only_steps_carrying_one_of_them() {
        let f = filter(Some("web,db"), None);
        assert_eq!(f.excludes(&tags(&["db", "slow"])), None);
        assert_eq!(
            f.excludes(&tags(&["cache"])).as_deref(),
            Some("not in --tags web,db")
        );
        assert!(
            f.excludes(&[]).is_some(),
            "an untagged step is not selected"
        );
    }

    #[test]
    fn always_runs_unless_skipped_by_name() {
        assert_eq!(filter(Some("web"), None).excludes(&tags(&[ALWAYS])), None);
        assert!(
            filter(None, Some(ALWAYS))
                .excludes(&tags(&[ALWAYS]))
                .is_some()
        );
    }

    #[test]
    fn skip_tags_win_over_tags() {
        let f = filter(Some("web"), Some("slow"));
        assert_eq!(
            f.excludes(&tags(&["web", "slow"])).as_deref(),
            Some("--skip-tags slow")
        );
        assert_eq!(filter(None, Some("slow")).excludes(&[]), None);
    }

    /// With inventory `plan=`s, several plans run at once; a tag any of them uses is known.
    #[test]
    fn a_tag_is_known_if_any_plan_that_runs_uses_it() {
        let web =
            crate::config::parse_plan(r#"plan "web" { step "a" tags="web" { shell "true" } }"#)
                .unwrap();
        let db = crate::config::parse_plan(r#"plan "db" { step "b" tags="db" { shell "true" } }"#)
            .unwrap();
        assert!(
            filter(Some("web,db"), None)
                .check_known([&web, &db].into_iter())
                .is_ok()
        );
    }

    #[test]
    fn with_no_tags_anywhere_the_error_says_so() {
        let plan = crate::config::parse_plan(r#"plan "p" { step "a" { shell "true" } }"#).unwrap();
        let err = filter(None, Some("slow"))
            .check_known([&plan].into_iter())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no step has tags"), "{err}");
    }

    #[test]
    fn a_tag_no_step_carries_is_an_error() {
        let plan = crate::config::parse_plan(
            r#"plan "p" {
                step "a" tags="web" { shell "true" }
                step "b" tags="db,web" { shell "true" }
            }"#,
        )
        .unwrap();
        assert!(
            filter(Some("web"), Some("db"))
                .check_known([&plan].into_iter())
                .is_ok()
        );
        let err = filter(Some("wbe"), None)
            .check_known([&plan].into_iter())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'wbe'") && err.contains("tags in use: db, web"),
            "{err}"
        );
    }
}
