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
/// Two-pass process:
/// 1. Expand `${for binding in collection}...${endfor}` blocks using `data.collections`
/// 2. Interpolate remaining `${var}` references using `vars`
pub fn render(
    template: &str,
    vars: &HashMap<String, String>,
    data: &TemplateData,
) -> Result<String, GlideshError> {
    let expanded = expand_for_blocks(template, data)?;
    if data.extra_vars.is_empty() {
        interpolate(&expanded, vars)
    } else {
        // User vars first, then extra_vars override — this prevents user-defined
        // keys from spoofing reserved @inventory.*/@group.* references.
        let mut merged = vars.clone();
        merged.extend(data.extra_vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        interpolate(&expanded, &merged)
    }
}

/// Expand all `${for <binding> in <collection>}...${endfor}` blocks.
///
/// For each block, iterates over the named collection and resolves `${binding.field}`
/// references from the current item. Other `${...}` references are left untouched
/// for the subsequent `interpolate()` pass.
fn expand_for_blocks(template: &str, data: &TemplateData) -> Result<String, GlideshError> {
    let mut result = template.to_string();

    while let Some(for_start) = result.find("${for ") {
        let header_end =
            result[for_start..]
                .find('}')
                .ok_or_else(|| GlideshError::TemplateError {
                    message: "Unclosed ${for ...} tag".to_string(),
                })?
                + for_start;

        let header = &result[for_start + 2..header_end]; // "for binding in collection ..."
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 4 || parts[0] != "for" || parts[2] != "in" {
            return Err(GlideshError::TemplateError {
                message: format!(
                    "Invalid for-loop syntax: expected '${{for <binding> in <collection>}}', got '${{{}}}' ",
                    header
                ),
            });
        }
        let binding = parts[1];
        // The for-binding introduces `${binding.*}` into the loop body's scope, so it is a
        // variable-name declaration like any other: it may not shadow a reserved `@` namespace.
        if binding.starts_with('@') {
            return Err(GlideshError::TemplateError {
                message: format!(
                    "for-loop binding '{}' cannot use the reserved '@' namespace",
                    binding
                ),
            });
        }
        let collection_name = parts[3];

        // Parse optional separator="..." from the header
        let separator = if let Some(start) = header.find("separator=\"") {
            let val_start = start + "separator=\"".len();
            let val_end =
                header[val_start..]
                    .find('"')
                    .ok_or_else(|| GlideshError::TemplateError {
                        message: "Unclosed separator value in for-loop (missing closing quote)"
                            .to_string(),
                    })?
                    + val_start;
            Some(&header[val_start..val_end])
        } else {
            None
        };

        // Find matching ${endfor}, tracking nesting depth
        let body_start = header_end + 1;
        let mut depth = 1u32;
        let mut search_pos = body_start;
        let mut endfor_start = None;

        while depth > 0 {
            let next_for = result[search_pos..].find("${for ");
            let next_endfor = result[search_pos..].find("${endfor}");

            match (next_for, next_endfor) {
                (_, None) => {
                    return Err(GlideshError::TemplateError {
                        message: format!("Missing ${{endfor}} for loop over '{}'", collection_name),
                    });
                }
                (Some(f), Some(e)) if f < e => {
                    depth += 1;
                    search_pos += f + 6; // skip past "${for "
                }
                (_, Some(e)) => {
                    depth -= 1;
                    if depth == 0 {
                        endfor_start = Some(search_pos + e);
                    } else {
                        search_pos += e + 9; // skip past "${endfor}"
                    }
                }
            }
        }

        let endfor_start = endfor_start.unwrap();
        let endfor_end = endfor_start + 9; // "${endfor}".len()

        let body = &result[body_start..endfor_start];

        let items =
            data.collections
                .get(collection_name)
                .ok_or_else(|| GlideshError::TemplateError {
                    message: format!("Undefined collection in for-loop: {}", collection_name),
                })?;

        let binding_prefix = format!("${{{binding}."); // "${binding."
        let mut rendered_items: Vec<String> = Vec::new();

        for item in items {
            let mut line = body.to_string();
            while let Some(ref_start) = line.find(&binding_prefix) {
                let ref_end =
                    line[ref_start..]
                        .find('}')
                        .ok_or_else(|| GlideshError::TemplateError {
                            message: format!("Unclosed ${{{}.…}} reference", binding),
                        })?
                        + ref_start;

                let field = &line[ref_start + binding_prefix.len()..ref_end];
                let value = item.get(field).ok_or_else(|| GlideshError::TemplateError {
                    message: format!(
                        "Undefined field '{}' in collection '{}' (available: {})",
                        field,
                        collection_name,
                        item.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                })?;

                line = format!("{}{}{}", &line[..ref_start], value, &line[ref_end + 1..]);
            }
            rendered_items.push(line);
        }

        let expanded = match separator {
            Some(sep) => rendered_items.join(sep),
            None => rendered_items.concat(),
        };

        result = format!(
            "{}{}{}",
            &result[..for_start],
            expanded,
            &result[endfor_end..]
        );
    }

    Ok(result)
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

/// Interpolate `${var-name}` patterns in a string using the provided variables.
pub fn interpolate(template: &str, vars: &HashMap<String, String>) -> Result<String, GlideshError> {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut found_close = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    found_close = true;
                    break;
                }
                var_name.push(ch);
            }
            if !found_close {
                return Err(GlideshError::TemplateError {
                    message: format!("Unclosed variable reference: ${{{}", var_name),
                });
            }
            match vars.get(&var_name) {
                Some(value) => result.push_str(value),
                None => {
                    return Err(GlideshError::TemplateError {
                        message: format!("Undefined variable: {}", var_name),
                    });
                }
            }
        } else {
            result.push(ch);
        }
    }

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

    fn refs(content: &str, defined: &[&str]) -> Vec<String> {
        defined_references(content.as_bytes(), |n| defined.contains(&n))
    }

    #[test]
    fn only_names_glidesh_defines_are_reported() {
        let env = "CUDA_VISIBLE_DEVICES=${cuda-devices}\nHOME_DIR=${HOME}\n";
        assert_eq!(refs(env, &["cuda-devices"]), ["cuda-devices"]);
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
}
