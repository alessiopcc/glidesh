use crate::error::GlideshError;
use std::collections::HashMap;

/// Static empty template data for contexts that don't need loops or inventory refs.
pub static EMPTY_TEMPLATE_DATA: std::sync::LazyLock<TemplateData> =
    std::sync::LazyLock::new(TemplateData::default);

/// Structured template data for loop expansion and inventory references.
#[derive(Debug, Clone, Default)]
pub struct TemplateData {
    /// Named collections for `${for item in collection}` loops.
    /// Each collection is a list of maps with string fields accessible via `${binding.field}`.
    pub collections: HashMap<String, Vec<HashMap<String, String>>>,
    /// Extra flat vars (e.g., `@inventory.host.address`) injected alongside user vars.
    pub extra_vars: HashMap<String, String>,
}

/// Render a template with full support for `${for}` loops and `${var}` interpolation.
///
/// One pass over the template: `${for binding in collection}…${endfor}` repeats its body
/// for each item of `data.collections`, `${binding.field}` reads the item, and any other
/// `${name}` reads `data.extra_vars`, then `vars` — so user variables cannot spoof the
/// reserved `@inventory.*`/`@group.*` ones. A value is written as it is, never read as
/// template text.
///
/// `$${` writes a literal `${`: `$${HOME}` renders as `${HOME}`, and is no reference, nor
/// a loop.
pub fn render(
    template: &str,
    vars: &HashMap<String, String>,
    data: &TemplateData,
) -> Result<String, GlideshError> {
    render_at(template, vars, data).map_err(|(_, message)| GlideshError::TemplateError { message })
}

/// [`render`] for a template file: an error names `source` and the line of the `${…}` it
/// is about.
pub fn render_file(
    template: &str,
    vars: &HashMap<String, String>,
    data: &TemplateData,
    source: &str,
) -> Result<String, GlideshError> {
    render_at(template, vars, data).map_err(|(at, message)| {
        let line = template[..at].matches('\n').count() + 1;
        let message = match message.strip_prefix("Undefined variable: ") {
            Some(name) => format!(
                "template {source}, line {line}: undefined variable {name}{}",
                literal_hint(name)
            ),
            None => format!("template {source}, line {line}: {message}"),
        };
        GlideshError::TemplateError { message }
    })
}

/// For a name that reads like the shell's rather than glidesh's — upper case, or holding a
/// character a glidesh variable does not (`${VAR:-default}`) — how to write it literally.
pub fn literal_hint(name: &str) -> String {
    let shell_like = !name.chars().any(|c| c.is_ascii_lowercase())
        || name.contains(|c: char| !(c.is_ascii_alphanumeric() || "-_.@".contains(c)));
    if shell_like {
        format!(" (if it is meant for the shell, write $${{{name}}} to keep ${{{name}}} as is)")
    } else {
        String::new()
    }
}

/// A `${…}` in a template, as [`render`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token<'a> {
    /// `${name}`: a variable, or `${binding.field}` inside a loop over `binding`.
    Var(&'a str),
    /// `$${name}`: a literal `${name}`.
    Escaped(&'a str),
    /// `${for binding in collection}`, with its `separator="…"`.
    For {
        binding: &'a str,
        collection: &'a str,
        separator: Option<&'a str>,
    },
    /// `${endfor}`.
    EndFor,
    /// A `${for …}` that cannot be read, and why.
    Invalid(String),
}

/// Every [`Token`] in `template`, with its 1-based line, in order — for `validate`, which
/// checks a template without rendering it. A `${` never closed ends the list, as it fails
/// the render.
pub fn tokens(template: &str) -> Vec<(usize, Token<'_>)> {
    let (found, _) = lex(template);
    let (mut line, mut counted) = (1, 0);
    found
        .into_iter()
        .map(|(at, _, token)| {
            line += template[counted..at].matches('\n').count();
            counted = at;
            (line, token)
        })
        .collect()
}

/// The field `name` reads from a loop over `binding` — `${h.address}` in `${for h …}` —
/// or `None`. A binding may itself hold a dot, so it is matched as a whole prefix.
pub fn bound_field<'a>(name: &'a str, binding: &str) -> Option<&'a str> {
    name.strip_prefix(binding)?.strip_prefix('.')
}

/// Why `template` cannot be rendered whatever the variables — an unclosed `${`, a
/// `${for}` that cannot be read or has no `${endfor}`, a reserved binding — with the line.
pub fn structure_error(template: &str) -> Option<(usize, String)> {
    parse(template)
        .err()
        .map(|(at, message)| (template[..at].matches('\n').count() + 1, message))
}

/// A render error, at the byte offset of the `${…}` it is about.
type AtError = (usize, String);

/// Each `${…}` of `template` with its byte offset and length, in order, and the error of a
/// `${` never closed, which ends the scan.
fn lex(template: &str) -> (Vec<(usize, usize, Token<'_>)>, Option<AtError>) {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = template[from..].find('$') {
        let at = from + offset;
        let rest = &template[at..];
        if let Some(after) = rest.strip_prefix("$${") {
            let name = after.find('}').map_or("", |end| &after[..end]);
            found.push((at, 3, Token::Escaped(name)));
            // What follows `$${` is text, and a `${` in it is a reference again.
            from = at + 3;
            continue;
        }
        let Some(after) = rest.strip_prefix("${") else {
            from = at + 1;
            continue;
        };
        let Some(end) = after.find('}') else {
            let message = if after.starts_with("for ") {
                "Unclosed ${for ...} tag".to_string()
            } else {
                format!("Unclosed variable reference: ${{{after}")
            };
            return (found, Some((at, message)));
        };
        let inner = &after[..end];
        let token = if inner.starts_with("for ") {
            for_header(inner)
        } else if inner == "endfor" {
            Token::EndFor
        } else {
            Token::Var(inner)
        };
        found.push((at, 2 + end + 1, token));
        from = at + 2 + end + 1;
    }
    (found, None)
}

/// A `${for binding in collection [separator="…"]}` header, without its `${` and `}`.
fn for_header(header: &str) -> Token<'_> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 4 || parts[2] != "in" {
        return Token::Invalid(format!(
            "Invalid for-loop syntax: expected '${{for <binding> in <collection>}}', got '${{{header}}}'"
        ));
    }
    let separator = match header.find("separator=\"") {
        Some(start) => {
            let value = &header[start + "separator=\"".len()..];
            match value.find('"') {
                Some(end) => Some(&value[..end]),
                None => {
                    return Token::Invalid(
                        "Unclosed separator value in for-loop (missing closing quote)".to_string(),
                    );
                }
            }
        }
        None => None,
    };
    Token::For {
        binding: parts[1],
        collection: parts[3],
        separator,
    }
}

/// A template read into its loops.
enum Node<'a> {
    Text(&'a str),
    Var {
        at: usize,
        name: &'a str,
    },
    For {
        at: usize,
        binding: &'a str,
        collection: &'a str,
        separator: Option<&'a str>,
        body: Vec<Node<'a>>,
    },
}

fn parse(template: &str) -> Result<Vec<Node<'_>>, AtError> {
    struct Open<'a> {
        at: usize,
        binding: &'a str,
        collection: &'a str,
        separator: Option<&'a str>,
        outer: Vec<Node<'a>>,
    }
    let (found, unclosed) = lex(template);
    let mut open: Vec<Open> = Vec::new();
    let mut nodes = Vec::new();
    let mut text_from = 0;
    for (at, len, token) in found {
        if at > text_from {
            nodes.push(Node::Text(&template[text_from..at]));
        }
        text_from = at + len;
        match token {
            Token::Escaped(_) => nodes.push(Node::Text("${")),
            Token::Var(name) => nodes.push(Node::Var { at, name }),
            Token::Invalid(message) => return Err((at, message)),
            // The binding introduces `${binding.*}` into the loop body's scope, so it is a
            // variable-name declaration like any other: it may not shadow a reserved `@` one.
            Token::For { binding, .. } if binding.starts_with('@') => {
                return Err((
                    at,
                    format!("for-loop binding '{binding}' cannot use the reserved '@' namespace"),
                ));
            }
            Token::For {
                binding,
                collection,
                separator,
            } => open.push(Open {
                at,
                binding,
                collection,
                separator,
                outer: std::mem::take(&mut nodes),
            }),
            Token::EndFor => match open.pop() {
                Some(loop_) => {
                    let body = std::mem::replace(&mut nodes, loop_.outer);
                    nodes.push(Node::For {
                        at: loop_.at,
                        binding: loop_.binding,
                        collection: loop_.collection,
                        separator: loop_.separator,
                        body,
                    });
                }
                None => nodes.push(Node::Var { at, name: "endfor" }),
            },
        }
    }
    if let Some(error) = unclosed {
        return Err(error);
    }
    if let Some(loop_) = open.pop() {
        return Err((
            loop_.at,
            format!("Missing ${{endfor}} for loop over '{}'", loop_.collection),
        ));
    }
    if text_from < template.len() {
        nodes.push(Node::Text(&template[text_from..]));
    }
    Ok(nodes)
}

/// The loops a node sits in, innermost last: binding, collection, and the current item.
type Bindings<'a> = Vec<(&'a str, &'a str, &'a HashMap<String, String>)>;

fn render_at(
    template: &str,
    vars: &HashMap<String, String>,
    data: &TemplateData,
) -> Result<String, AtError> {
    let nodes = parse(template)?;
    let mut out = String::with_capacity(template.len());
    write_nodes(&nodes, vars, data, &Vec::new(), &mut out)?;
    Ok(out)
}

fn write_nodes<'a>(
    nodes: &'a [Node<'a>],
    vars: &'a HashMap<String, String>,
    data: &'a TemplateData,
    bindings: &Bindings<'a>,
    out: &mut String,
) -> Result<(), AtError> {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Var { at, name } => {
                let item = bindings
                    .iter()
                    .rev()
                    .find_map(|(binding, collection, item)| {
                        bound_field(name, binding).map(|field| (field, *collection, *item))
                    });
                let value = match item {
                    Some((field, collection, item)) => item.get(field).ok_or_else(|| {
                        let mut fields: Vec<&str> = item.keys().map(String::as_str).collect();
                        fields.sort_unstable();
                        (
                            *at,
                            format!(
                                "Undefined field '{field}' in collection '{collection}' \
                                 (available: {})",
                                fields.join(", ")
                            ),
                        )
                    })?,
                    None => data
                        .extra_vars
                        .get(*name)
                        .or_else(|| vars.get(*name))
                        .ok_or_else(|| (*at, format!("Undefined variable: {name}")))?,
                };
                out.push_str(value);
            }
            Node::For {
                at,
                binding,
                collection,
                separator,
                body,
            } => {
                let items = data.collections.get(*collection).ok_or_else(|| {
                    (
                        *at,
                        format!("Undefined collection in for-loop: {collection}"),
                    )
                })?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(separator.unwrap_or(""));
                    }
                    let mut inner = bindings.clone();
                    inner.push((binding, collection, item));
                    write_nodes(body, vars, data, &inner, out)?;
                }
            }
        }
    }
    Ok(())
}

/// The `${name}` references in `content` whose name `is_defined` accepts, sorted and without
/// duplicates.
///
/// Used to warn when a file uploaded without `template #true` would ship a reference
/// literally. Only names glidesh defines count: a shell script's `${HOME}` is not a glidesh
/// variable and must not be flagged. Works on bytes, since a plain upload may be binary.
pub fn defined_references(content: &[u8], is_defined: impl Fn(&str) -> bool) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    let mut rest = content;
    while let Some(start) = rest.windows(2).position(|w| w == b"${") {
        let after = &rest[start + 2..];
        if start > 0 && rest[start - 1] == b'$' {
            rest = after;
            continue;
        }
        let Some(end) = after.iter().position(|&b| b == b'}') else {
            break;
        };
        // `${for …}` already fails the whitespace test; `${endfor}` is the other directive.
        let name = std::str::from_utf8(&after[..end]).ok().filter(|n| {
            !n.is_empty() && *n != "endfor" && !n.contains(|c: char| c.is_whitespace() || c == '$')
        });
        match name {
            Some(name) => {
                if is_defined(name) {
                    found.insert(name.to_string());
                }
                rest = &after[end + 1..];
            }
            // Not a reference; rescan from just past this `${` so one nested inside it is
            // still seen.
            None => rest = after,
        }
    }
    found.into_iter().collect()
}

/// Interpolate `${var-name}` patterns in a string using the provided variables. `$${`
/// writes a literal `${`.
pub fn interpolate(template: &str, vars: &HashMap<String, String>) -> Result<String, GlideshError> {
    let mut result = String::with_capacity(template.len());
    let mut at = 0;
    while let Some(offset) = template[at..].find('$') {
        let start = at + offset;
        result.push_str(&template[at..start]);
        let rest = &template[start..];
        if rest.starts_with("$${") {
            result.push_str("${");
            at = start + 3;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err(GlideshError::TemplateError {
                    message: format!("Unclosed variable reference: ${{{}", after),
                });
            };
            let var_name = &after[..end];
            match vars.get(var_name) {
                Some(value) => result.push_str(value),
                None => {
                    return Err(GlideshError::TemplateError {
                        message: format!("Undefined variable: {}", var_name),
                    });
                }
            }
            at = start + 2 + end + 1;
        } else {
            result.push('$');
            at = start + 1;
        }
    }
    result.push_str(&template[at..]);
    Ok(result)
}

/// Interpolate all string values in a ParamValue map.
pub fn interpolate_args(
    args: &HashMap<String, crate::config::types::ParamValue>,
    vars: &HashMap<String, String>,
) -> Result<HashMap<String, crate::config::types::ParamValue>, GlideshError> {
    use crate::config::types::ParamValue;

    let mut result = HashMap::new();
    for (key, value) in args {
        let new_value = match value {
            ParamValue::String(s) => ParamValue::String(interpolate(s, vars)?),
            ParamValue::List(list) => {
                let new_list: Result<Vec<String>, _> =
                    list.iter().map(|s| interpolate(s, vars)).collect();
                ParamValue::List(new_list?)
            }
            ParamValue::Map(map) => {
                let mut new_map = HashMap::new();
                for (mk, mv) in map {
                    new_map.insert(mk.clone(), interpolate(mv, vars)?);
                }
                ParamValue::Map(new_map)
            }
            other => other.clone(),
        };
        result.insert(key.clone(), new_value);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value that looks like a reference or a loop, such as an error message quoting a
    /// command, is written as it is: rendering what it holds could fail the task, or loop.
    #[test]
    fn a_rendered_value_is_never_expanded_again() {
        let vars = HashMap::from([(
            "@error.msg".to_string(),
            "echo ${secret} ${for x in xs}${x}${endfor}".to_string(),
        )]);
        let out = render("failed: ${@error.msg}", &vars, &TemplateData::default()).unwrap();
        assert_eq!(out, "failed: echo ${secret} ${for x in xs}${x}${endfor}");
    }

    fn refs(content: &str, defined: &[&str]) -> Vec<String> {
        defined_references(content.as_bytes(), |n| defined.contains(&n))
    }

    #[test]
    fn only_names_glidesh_defines_are_reported() {
        let env = "CUDA_VISIBLE_DEVICES=${cuda-devices}\nHOME_DIR=${HOME}\n";
        assert_eq!(refs(env, &["cuda-devices"]), ["cuda-devices"]);
    }

    #[test]
    fn an_escaped_reference_is_not_reported() {
        assert!(refs("a=$${cuda-devices}", &["cuda-devices"]).is_empty());
        assert_eq!(refs("$${x} ${x}", &["x"]), ["x"]);
    }

    #[test]
    fn references_are_sorted_and_deduplicated() {
        let content = "${b} ${a} ${b} ${@host.name}";
        assert_eq!(
            refs(content, &["a", "b", "@host.name"]),
            ["@host.name", "a", "b"]
        );
    }

    #[test]
    fn template_directives_and_malformed_references_are_ignored() {
        let content = "${for h in hosts}${h.name}${endfor} ${ spaced } ${} ${unclosed";
        assert!(refs(content, &["hosts", "for", "endfor", "spaced"]).is_empty());
    }

    #[test]
    fn a_reference_inside_a_malformed_one_is_still_found() {
        assert_eq!(refs("${ ${cuda}}", &["cuda"]), ["cuda"]);
    }

    #[test]
    fn binary_content_is_scanned_without_failing() {
        let mut content = vec![0xff, 0xfe, 0x00];
        content.extend_from_slice(b"${port}");
        content.push(0xff);
        assert_eq!(defined_references(&content, |n| n == "port"), ["port"]);
    }

    #[test]
    fn test_simple_interpolation() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        assert_eq!(interpolate("hello ${name}", &vars).unwrap(), "hello world");
    }

    #[test]
    fn test_multiple_vars() {
        let mut vars = HashMap::new();
        vars.insert("host".to_string(), "localhost".to_string());
        vars.insert("port".to_string(), "8080".to_string());
        assert_eq!(
            interpolate("http://${host}:${port}/api", &vars).unwrap(),
            "http://localhost:8080/api"
        );
    }

    #[test]
    fn test_no_vars() {
        let vars = HashMap::new();
        assert_eq!(
            interpolate("no variables here", &vars).unwrap(),
            "no variables here"
        );
    }

    #[test]
    fn test_undefined_var() {
        let vars = HashMap::new();
        assert!(interpolate("${undefined}", &vars).is_err());
    }

    #[test]
    fn test_unclosed_var() {
        let vars = HashMap::new();
        assert!(interpolate("${unclosed", &vars).is_err());
    }

    #[test]
    fn test_render_for_loop_basic() {
        let mut vars = HashMap::new();
        vars.insert("title".to_string(), "Config".to_string());

        let mut data = TemplateData::default();
        data.collections.insert(
            "items".to_string(),
            vec![
                HashMap::from([
                    ("name".to_string(), "a".to_string()),
                    ("value".to_string(), "1".to_string()),
                ]),
                HashMap::from([
                    ("name".to_string(), "b".to_string()),
                    ("value".to_string(), "2".to_string()),
                ]),
            ],
        );

        let template = "# ${title}\n${for x in items}\n${x.name}=${x.value}\n${endfor}";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "# Config\n\na=1\n\nb=2\n");
    }

    #[test]
    fn test_render_preserves_simple_vars() {
        let mut vars = HashMap::new();
        vars.insert("host".to_string(), "10.0.0.1".to_string());
        let data = TemplateData::default();

        let template = "server ${host}";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "server 10.0.0.1");
    }

    #[test]
    fn test_render_empty_collection() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert("items".to_string(), vec![]);

        let template = "before\n${for x in items}${x.name}\n${endfor}after";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "before\nafter");
    }

    #[test]
    fn for_binding_cannot_use_reserved_at_namespace() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert(
            "items".to_string(),
            vec![HashMap::from([("name".to_string(), "a".to_string())])],
        );
        let template = "${for @host in items}${@host.name}${endfor}";
        let err = render(template, &vars, &data).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn test_render_missing_collection() {
        let vars = HashMap::new();
        let data = TemplateData::default();

        let template = "${for x in missing}${x.name}${endfor}";
        let result = render(template, &vars, &data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn test_render_missing_field() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert(
            "items".to_string(),
            vec![HashMap::from([("name".to_string(), "a".to_string())])],
        );

        let template = "${for x in items}${x.nonexistent}${endfor}";
        let result = render(template, &vars, &data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn test_render_unclosed_for() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert("items".to_string(), vec![]);

        let template = "${for x in items}no endfor here";
        let result = render(template, &vars, &data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("endfor"));
    }

    #[test]
    fn test_render_no_for_blocks() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        let data = TemplateData::default();

        let result = render("hello ${name}", &vars, &data).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_render_for_loop_separator() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert(
            "items".to_string(),
            vec![
                HashMap::from([("name".to_string(), "a".to_string())]),
                HashMap::from([("name".to_string(), "b".to_string())]),
                HashMap::from([("name".to_string(), "c".to_string())]),
            ],
        );

        let template = "[\n${for x in items separator=\",\"}\n  \"${x.name}\"\n${endfor}\n]";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "[\n\n  \"a\"\n,\n  \"b\"\n,\n  \"c\"\n\n]");
    }

    #[test]
    fn test_render_for_loop_separator_single_item() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert(
            "items".to_string(),
            vec![HashMap::from([("name".to_string(), "only".to_string())])],
        );

        let template = "${for x in items separator=\",\"}${x.name}${endfor}";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "only");
    }

    #[test]
    fn test_render_for_loop_separator_empty() {
        let vars = HashMap::new();
        let mut data = TemplateData::default();
        data.collections.insert("items".to_string(), vec![]);

        let template = "[${for x in items separator=\",\"}${x.name}${endfor}]";
        let result = render(template, &vars, &data).unwrap();
        assert_eq!(result, "[]");
    }

    #[test]
    fn an_escaped_reference_is_written_literally_next_to_a_real_one() {
        let vars = HashMap::from([("name".to_string(), "web".to_string())]);
        assert_eq!(
            interpolate("home=$${HOME} host=${name} $$${x}", &vars).unwrap(),
            "home=${HOME} host=web $${x}"
        );
        assert_eq!(interpolate("cost $5, $${}", &vars).unwrap(), "cost $5, ${}");
    }

    #[test]
    fn a_binding_holding_a_dot_reads_its_fields() {
        let data = TemplateData {
            collections: HashMap::from([(
                "hosts".to_string(),
                vec![HashMap::from([("name".to_string(), "a".to_string())])],
            )]),
            ..TemplateData::default()
        };
        assert_eq!(
            render(
                "${for host.item in hosts}${host.item.name}${endfor}",
                &HashMap::new(),
                &data
            )
            .unwrap(),
            "a"
        );
    }

    #[test]
    fn an_escaped_loop_is_text_and_a_loop_body_keeps_its_escapes() {
        let data = TemplateData {
            collections: HashMap::from([(
                "hosts".to_string(),
                vec![HashMap::from([("name".to_string(), "a".to_string())])],
            )]),
            ..TemplateData::default()
        };
        let template = "$${for x in y}$${endfor}\n\
                        ${for h in hosts}${h.name} $${h.name} $${HOME}${endfor}";
        assert_eq!(
            render(template, &HashMap::new(), &data).unwrap(),
            "${for x in y}${endfor}\na ${h.name} ${HOME}"
        );
    }

    #[test]
    fn tokens_are_read_as_render_reads_them() {
        let found = tokens("a ${x}\n$${HOME}\n${for h in @group.web}${h.addr}${endfor} $${a${b}}");
        assert_eq!(
            found,
            [
                (1, Token::Var("x")),
                (2, Token::Escaped("HOME")),
                (
                    3,
                    Token::For {
                        binding: "h",
                        collection: "@group.web",
                        separator: None,
                    }
                ),
                (3, Token::Var("h.addr")),
                (3, Token::EndFor),
                (3, Token::Escaped("a${b")),
                (3, Token::Var("b")),
            ]
        );
    }

    #[test]
    fn an_undefined_variable_in_a_template_file_names_the_file_and_line() {
        let err = render_file(
            "one\ntwo ${PATH}\n",
            &HashMap::new(),
            &TemplateData::default(),
            "files/run.sh",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("template files/run.sh, line 2: undefined variable PATH"),
            "{err}"
        );
        assert!(err.contains("write $${PATH}"), "{err}");

        let err = render_file(
            "${db-host}",
            &HashMap::new(),
            &TemplateData::default(),
            "app.conf",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("line 1: undefined variable db-host"), "{err}");
        assert!(
            !err.contains("$${"),
            "a glidesh-like name gets no shell hint: {err}"
        );
    }

    #[test]
    fn a_value_ending_in_a_dollar_does_not_escape_what_follows() {
        let data = TemplateData {
            collections: HashMap::from([(
                "c".to_string(),
                vec![HashMap::from([
                    ("a".to_string(), "x$".to_string()),
                    ("b".to_string(), "B".to_string()),
                ])],
            )]),
            ..TemplateData::default()
        };
        let vars = HashMap::from([
            ("s".to_string(), "S".to_string()),
            ("p".to_string(), "pa$".to_string()),
        ]);
        assert_eq!(
            render(
                "${for h in c}${h.a}${h.b}|${h.a}${s}${endfor} ${p}${s}",
                &vars,
                &data
            )
            .unwrap(),
            "x$B|x$S pa$S"
        );
    }

    #[test]
    fn an_error_points_at_the_reference_it_is_about() {
        let data = TemplateData {
            collections: HashMap::from([(
                "c".to_string(),
                vec![HashMap::from([("b".to_string(), "B".to_string())])],
            )]),
            ..TemplateData::default()
        };
        let err = render_file(
            "${for h in c}${h.b}${endfor}\n\n${h.b}\n",
            &HashMap::new(),
            &data,
            "a.conf",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("template a.conf, line 3: undefined variable h.b"),
            "{err}"
        );

        let err = render_file(
            "ok\n${for h in c}\n${h.missing}${endfor}",
            &HashMap::new(),
            &data,
            "a.conf",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("line 3: Undefined field 'missing' in collection 'c' (available: b)"),
            "{err}"
        );

        let err = render_file("\n${for x of c}", &HashMap::new(), &data, "a.conf")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2: Invalid for-loop syntax"), "{err}");
    }
}
