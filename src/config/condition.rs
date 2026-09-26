//! `when=` conditions on steps and tasks.
//!
//! The grammar is deliberately tiny — string equality, presence, truthiness, `&&`, `||` and a
//! leading `!` — so plans stay declarative rather than growing a templating language:
//!
//! ```text
//! or      := and ( "||" and )*
//! and     := term ( "&&" term )*
//! term    := "!"? atom
//! atom    := "defined" var | "undefined" var | operand ( ("==" | "!=") operand )?
//! operand := var | 'quoted literal' | bare-word
//! var     := "${" name "}"
//! ```
//!
//! `&&` binds tighter than `||`, both short-circuit, and there are no parentheses.

use crate::error::GlideshError;
use std::collections::{HashMap, HashSet};

/// A parsed `when=` expression. Parsed with the plan, so a malformed condition fails
/// `glidesh validate` rather than a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    source: String,
    any: Vec<Vec<Term>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Term {
    negated: bool,
    atom: Atom,
}

#[derive(Debug, Clone, PartialEq)]
enum Atom {
    Defined(String),
    Undefined(String),
    Truthy(Operand),
    Compare(Operand, Op, Operand),
}

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Var(String),
    Literal(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
}

/// The result of evaluating a condition against a host's variables.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    True,
    False,
    /// The answer depends on the *value* of a variable a preview cannot know — one that a
    /// task registered earlier in the same `--dry-run`, where registration captures nothing.
    Undetermined(String),
}

impl Condition {
    pub fn parse(source: &str) -> Result<Self, GlideshError> {
        let fail = |message: String| GlideshError::ConfigParse {
            message: format!("invalid when=\"{source}\": {message}"),
        };
        let tokens = tokenize(source).map_err(fail)?;
        let any = Parser { tokens, pos: 0 }.parse().map_err(fail)?;
        Ok(Self {
            source: source.to_string(),
            any,
        })
    }

    /// The expression as written in the plan. Used as the reason a step or task was skipped,
    /// so it never carries an interpolated — possibly secret — value.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every variable the condition refers to.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.any.iter().flatten().flat_map(|term| {
            let (a, b) = match &term.atom {
                Atom::Defined(v) | Atom::Undefined(v) => (Some(v.as_str()), None),
                Atom::Truthy(op) => (op.var(), None),
                Atom::Compare(l, _, r) => (l.var(), r.var()),
            };
            a.into_iter().chain(b)
        })
    }

    /// Evaluate against a host's variables. `unknown` names variables whose *value* is not
    /// real — see [`Outcome::Undetermined`]. Their presence is still real: registering always
    /// defines the variable, so `defined`/`undefined` answer them normally.
    ///
    /// Reading an undefined variable is an error, as it is in interpolation. Short-circuiting
    /// is what lets `defined ${x} && ${x} == y` guard against it.
    pub fn eval(
        &self,
        vars: &HashMap<String, String>,
        unknown: &HashSet<String>,
    ) -> Result<Outcome, String> {
        // Kleene logic: an undetermined operand decides nothing on its own, but a later
        // definite answer can still settle the expression. Once undetermined, a later error
        // is reported as undetermined too, since the real run might never have reached it.
        let mut any_undetermined = None;
        for all in &self.any {
            match self.eval_all(all, vars, unknown) {
                Ok(Outcome::True) => return Ok(Outcome::True),
                Ok(Outcome::False) => {}
                Ok(Outcome::Undetermined(v)) => {
                    any_undetermined.get_or_insert(v);
                }
                Err(e) => return any_undetermined.map(Outcome::Undetermined).ok_or(e),
            }
        }
        Ok(any_undetermined.map_or(Outcome::False, Outcome::Undetermined))
    }

    fn eval_all(
        &self,
        all: &[Term],
        vars: &HashMap<String, String>,
        unknown: &HashSet<String>,
    ) -> Result<Outcome, String> {
        let mut undetermined = None;
        for term in all {
            match self.eval_term(term, vars, unknown) {
                Ok(Outcome::False) => return Ok(Outcome::False),
                Ok(Outcome::True) => {}
                Ok(Outcome::Undetermined(v)) => {
                    undetermined.get_or_insert(v);
                }
                Err(e) => return undetermined.map(Outcome::Undetermined).ok_or(e),
            }
        }
        Ok(undetermined.map_or(Outcome::True, Outcome::Undetermined))
    }

    fn eval_term(
        &self,
        term: &Term,
        vars: &HashMap<String, String>,
        unknown: &HashSet<String>,
    ) -> Result<Outcome, String> {
        let value = |op: &Operand| -> Result<Result<String, String>, String> {
            match op {
                Operand::Literal(s) => Ok(Ok(s.clone())),
                Operand::Var(v) if unknown.contains(v) => Ok(Err(v.clone())),
                Operand::Var(v) => vars.get(v).cloned().map(Ok).ok_or_else(|| {
                    format!(
                        "undefined variable '{v}' in when=\"{}\" — guard it with \
                         `defined ${{{v}}} && ...`",
                        self.source
                    )
                }),
            }
        };
        let holds = match &term.atom {
            Atom::Defined(v) => vars.contains_key(v),
            Atom::Undefined(v) => !vars.contains_key(v),
            Atom::Truthy(op) => match value(op)? {
                Ok(s) => !matches!(s.as_str(), "" | "false" | "0"),
                Err(v) => return Ok(Outcome::Undetermined(v)),
            },
            Atom::Compare(l, op, r) => {
                let (l, r) = (value(l)?, value(r)?);
                match (l, r) {
                    (Ok(l), Ok(r)) => (l == r) == (*op == Op::Eq),
                    (Err(v), _) | (_, Err(v)) => return Ok(Outcome::Undetermined(v)),
                }
            }
        };
        Ok(if holds != term.negated {
            Outcome::True
        } else {
            Outcome::False
        })
    }
}

impl Operand {
    fn var(&self) -> Option<&str> {
        match self {
            Operand::Var(v) => Some(v),
            Operand::Literal(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Var(String),
    Quoted(String),
    Word(String),
    Eq,
    Ne,
    And,
    Or,
    Not,
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::Var(v) => format!("'${{{v}}}'"),
            Token::Quoted(s) => format!("'{s}'"),
            Token::Word(w) => format!("'{w}'"),
            Token::Eq => "'=='".into(),
            Token::Ne => "'!='".into(),
            Token::And => "'&&'".into(),
            Token::Or => "'||'".into(),
            Token::Not => "'!'".into(),
        }
    }
}

fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = source.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '$' => {
                chars.next();
                if chars.next() != Some('{') {
                    return Err("expected '{' after '$'".into());
                }
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(format!("unclosed '${{{name}'")),
                    }
                }
                if name.is_empty() {
                    return Err("empty variable reference '${}'".into());
                }
                tokens.push(Token::Var(name));
            }
            '\'' => {
                chars.next();
                let mut text = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => text.push(c),
                        None => return Err(format!("unclosed quote '{text}")),
                    }
                }
                tokens.push(Token::Quoted(text));
            }
            '=' | '!' | '&' | '|' => {
                chars.next();
                let next = chars.peek().copied();
                let token = match (c, next) {
                    ('=', Some('=')) => Token::Eq,
                    ('!', Some('=')) => Token::Ne,
                    ('&', Some('&')) => Token::And,
                    ('|', Some('|')) => Token::Or,
                    ('!', _) => {
                        tokens.push(Token::Not);
                        continue;
                    }
                    ('=', _) => return Err("'=' is not an operator; use '=='".into()),
                    _ => return Err(format!("'{c}' is not an operator; use '{c}{c}'")),
                };
                chars.next();
                tokens.push(token);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "=!&|'$".contains(c) {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn parse(mut self) -> Result<Vec<Vec<Term>>, String> {
        if self.tokens.is_empty() {
            return Err("empty condition".into());
        }
        let mut any = vec![self.all()?];
        while self.eat(&Token::Or) {
            any.push(self.all()?);
        }
        match self.tokens.get(self.pos) {
            None => Ok(any),
            Some(t) => Err(format!(
                "unexpected {} — expected '==', '!=', '&&' or '||'",
                t.describe()
            )),
        }
    }

    fn all(&mut self) -> Result<Vec<Term>, String> {
        let mut all = vec![self.term()?];
        while self.eat(&Token::And) {
            all.push(self.term()?);
        }
        Ok(all)
    }

    fn term(&mut self) -> Result<Term, String> {
        let negated = self.eat(&Token::Not);
        if let Some(Token::Word(w)) = self.tokens.get(self.pos) {
            if w == "defined" || w == "undefined" {
                let keyword = w.clone();
                self.pos += 1;
                let var = match self.tokens.get(self.pos) {
                    Some(Token::Var(v)) => v.clone(),
                    _ => return Err(format!("'{keyword}' must be followed by a ${{variable}}")),
                };
                self.pos += 1;
                let atom = if keyword == "defined" {
                    Atom::Defined(var)
                } else {
                    Atom::Undefined(var)
                };
                return Ok(Term { negated, atom });
            }
        }
        let left = self.operand()?;
        let op = if self.eat(&Token::Eq) {
            Op::Eq
        } else if self.eat(&Token::Ne) {
            Op::Ne
        } else {
            return Ok(Term {
                negated,
                atom: Atom::Truthy(left),
            });
        };
        let right = self.operand()?;
        Ok(Term {
            negated,
            atom: Atom::Compare(left, op, right),
        })
    }

    fn operand(&mut self) -> Result<Operand, String> {
        let operand = match self.tokens.get(self.pos) {
            Some(Token::Var(v)) => Operand::Var(v.clone()),
            Some(Token::Quoted(s)) | Some(Token::Word(s)) => Operand::Literal(s.clone()),
            Some(t) => return Err(format!("expected a value, found {}", t.describe())),
            None => return Err("expected a value at the end of the condition".into()),
        };
        self.pos += 1;
        Ok(operand)
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.tokens.get(self.pos) == Some(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn eval(src: &str, pairs: &[(&str, &str)]) -> Result<Outcome, String> {
        Condition::parse(src)
            .unwrap()
            .eval(&vars(pairs), &HashSet::new())
    }

    fn holds(src: &str, pairs: &[(&str, &str)]) -> bool {
        match eval(src, pairs).unwrap() {
            Outcome::True => true,
            Outcome::False => false,
            other => panic!("{src}: unexpected {other:?}"),
        }
    }

    fn parse_err(src: &str) -> String {
        Condition::parse(src).unwrap_err().to_string()
    }

    #[test]
    fn equality_against_a_literal() {
        let family = [("@os.family", "debian")];
        assert!(holds("${@os.family} == debian", &family));
        assert!(!holds("${@os.family} == redhat", &family));
        assert!(holds("${@os.family} != redhat", &family));
        assert!(!holds("${@os.family} != debian", &family));
    }

    #[test]
    fn equality_between_two_variables() {
        assert!(holds("${a} == ${b}", &[("a", "x"), ("b", "x")]));
        assert!(!holds("${a} == ${b}", &[("a", "x"), ("b", "y")]));
    }

    #[test]
    fn quoted_literals_may_hold_spaces_and_operators() {
        let v = [("msg", "a == b && c")];
        assert!(holds("${msg} == 'a == b && c'", &v));
        assert!(holds("${e} == ''", &[("e", "")]));
    }

    #[test]
    fn only_empty_false_and_zero_are_falsy() {
        for falsy in ["", "false", "0"] {
            assert!(!holds("${v}", &[("v", falsy)]), "{falsy:?}");
        }
        for truthy in ["true", "1", "yes", "no", "False", "00", " "] {
            assert!(holds("${v}", &[("v", truthy)]), "{truthy:?}");
        }
    }

    #[test]
    fn presence() {
        assert!(holds("defined ${x}", &[("x", "")]));
        assert!(!holds("defined ${x}", &[]));
        assert!(holds("undefined ${x}", &[]));
        assert!(!holds("undefined ${x}", &[("x", "1")]));
    }

    #[test]
    fn negation() {
        assert!(holds("!${v}", &[("v", "false")]));
        assert!(holds("!defined ${x}", &[]));
        assert!(holds("! ${a} == b", &[("a", "c")]));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // a || (b && c), not (a || b) && c
        let v = [("a", "1"), ("b", "0"), ("c", "0")];
        assert!(holds("${a} || ${b} && ${c}", &v));
        let v = [("a", "0"), ("b", "1"), ("c", "0")];
        assert!(!holds("${a} || ${b} && ${c}", &v));
    }

    #[test]
    fn reading_an_undefined_variable_is_an_error() {
        let err = eval("${missing} == x", &[]).unwrap_err();
        assert!(err.contains("missing") && err.contains("defined"), "{err}");
        assert!(eval("${missing}", &[]).is_err());
    }

    /// Short-circuiting is the whole point of `defined ${x} && ...`: without it the guard
    /// could not protect the comparison after it.
    #[test]
    fn short_circuit_guards_an_undefined_variable() {
        assert!(!holds("defined ${x} && ${x} == y", &[]));
        assert!(holds("undefined ${x} || ${x} == y", &[]));
    }

    fn eval_unknown(src: &str, pairs: &[(&str, &str)], unknown: &[&str]) -> Outcome {
        let unknown = unknown.iter().map(|s| s.to_string()).collect();
        Condition::parse(src)
            .unwrap()
            .eval(&vars(pairs), &unknown)
            .unwrap()
    }

    #[test]
    fn a_value_not_yet_known_is_undetermined() {
        let out = [("out", "")];
        assert_eq!(
            eval_unknown("${out} == yes", &out, &["out"]),
            Outcome::Undetermined("out".into())
        );
        assert_eq!(
            eval_unknown("${out}", &out, &["out"]),
            Outcome::Undetermined("out".into())
        );
        assert_eq!(
            eval_unknown("!${out}", &out, &["out"]),
            Outcome::Undetermined("out".into())
        );
    }

    /// Registering always defines the variable, so its presence is known even when its
    /// value is not.
    #[test]
    fn presence_of_an_unknown_value_is_still_known() {
        let out = [("out", "")];
        assert_eq!(
            eval_unknown("defined ${out}", &out, &["out"]),
            Outcome::True
        );
    }

    #[test]
    fn a_definite_answer_settles_an_undetermined_one() {
        let v = [("out", ""), ("os", "debian")];
        assert_eq!(
            eval_unknown("${out} == yes && ${os} == redhat", &v, &["out"]),
            Outcome::False
        );
        assert_eq!(
            eval_unknown("${out} == yes || ${os} == debian", &v, &["out"]),
            Outcome::True
        );
        assert_eq!(
            eval_unknown("${out} == yes && ${os} == debian", &v, &["out"]),
            Outcome::Undetermined("out".into())
        );
    }

    /// The real run might short-circuit before the missing variable, so a preview must not
    /// report an error the run would never hit.
    #[test]
    fn an_error_after_an_undetermined_term_stays_undetermined() {
        let v = [("out", "")];
        assert_eq!(
            eval_unknown("${out} == yes && ${missing} == x", &v, &["out"]),
            Outcome::Undetermined("out".into())
        );
    }

    #[test]
    fn variables_lists_every_reference() {
        let c = Condition::parse("defined ${a} && ${b} == ${c} || !${d} || e == f").unwrap();
        let vars: Vec<&str> = c.variables().collect();
        assert_eq!(vars, ["a", "b", "c", "d"]);
    }

    #[test]
    fn source_is_kept_verbatim() {
        let src = "${@os.family}   ==  debian";
        assert_eq!(Condition::parse(src).unwrap().source(), src);
    }

    #[test]
    fn malformed_conditions_are_rejected() {
        for (src, expect) in [
            ("", "empty"),
            ("   ", "empty"),
            ("${a} ==", "end of the condition"),
            ("${a} &&", "end of the condition"),
            ("|| ${a}", "expected a value"),
            ("${a", "unclosed"),
            ("${}", "empty variable"),
            ("'open", "unclosed quote"),
            ("${a} = b", "use '=='"),
            ("${a} & ${b}", "use '&&'"),
            ("${a} | ${b}", "use '||'"),
            ("${a} > 3", "unexpected '>'"),
            ("${a} b", "unexpected 'b'"),
            ("defined a", "must be followed by a ${variable}"),
            ("$a", "expected '{'"),
        ] {
            let err = parse_err(src);
            assert!(err.contains(expect), "{src:?}: {err}");
            assert!(err.contains("invalid when="), "{src:?}: {err}");
        }
    }
}
