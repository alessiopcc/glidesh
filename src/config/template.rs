use crate::error::GlideshError;
use std::collections::HashMap;

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
}
