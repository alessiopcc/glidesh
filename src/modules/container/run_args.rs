//! Construction of the `<runtime> run` invocation and of the spec hash used to
//! detect configuration drift.
//!
//! The hash is derived from the generated argument list rather than from a
//! hand-maintained list of parameters, so a newly supported flag can never be
//! forgotten by the drift check.

use crate::config::types::ParamValue;
use crate::error::GlideshError;
use crate::modules::ModuleParams;
use crate::util::shell_escape;
use sha2::{Digest, Sha256};

pub(super) const PARAM_HASH_LABEL: &str = "sh.glide.param-hash";

/// Flags emitted as `--flag=value`.
const EQ_FLAGS: &[(&str, &str)] = &[
    ("cgroupns", "--cgroupns"),
    ("ipc", "--ipc"),
    ("pid", "--pid"),
    ("pull", "--pull"),
    ("restart", "--restart"),
    ("userns", "--userns"),
    ("uts", "--uts"),
];

/// Flags emitted as `--flag value`.
const VALUE_FLAGS: &[(&str, &str)] = &[
    ("cpus", "--cpus"),
    ("entrypoint", "--entrypoint"),
    ("hostname", "--hostname"),
    ("memory", "--memory"),
    ("network", "--network"),
    ("shm-size", "--shm-size"),
    ("stop-signal", "--stop-signal"),
    ("user", "--user"),
    ("workdir", "--workdir"),
];

/// Boolean switches emitted only when set to `#true`.
const BOOL_FLAGS: &[(&str, &str)] = &[
    ("init", "--init"),
    ("privileged", "--privileged"),
    ("read-only", "--read-only"),
];

/// List parameters, each element emitted as `--flag element`.
const LIST_FLAGS: &[(&str, &str)] = &[
    ("add-host", "--add-host"),
    ("cap-add", "--cap-add"),
    ("cap-drop", "--cap-drop"),
    ("devices", "--device"),
    ("dns", "--dns"),
    ("network-alias", "--network-alias"),
    ("ports", "-p"),
    ("security-opt", "--security-opt"),
    ("tmpfs", "--tmpfs"),
    ("volumes", "-v"),
];

/// Map parameters, each entry emitted as `--flag key=value` in sorted key order.
const MAP_FLAGS: &[(&str, &str)] = &[
    ("environment", "-e"),
    ("labels", "--label"),
    ("sysctls", "--sysctl"),
    ("ulimits", "--ulimit"),
];

/// Parameters glidesh interprets itself instead of forwarding to the runtime.
/// They stay out of the run arguments and therefore out of the spec hash: changing
/// a readiness probe must not force a healthy container to be recreated.
const GLIDESH_PARAMS: &[&str] = &[
    "check",
    "delay",
    "install-runtime",
    "ready-cmd",
    "remove",
    "retries",
    "runtime",
    "state",
    "success_codes",
    "timeout",
    "wait",
    "wait-interval",
    "wait-timeout",
];

/// Parameters handled by bespoke code in [`build_run_args`].
const SPECIAL_PARAMS: &[&str] = &["command", "extra-args", "gpus", "healthcheck", "image"];

/// Recognised keys inside a `healthcheck` block.
const HEALTHCHECK_KEYS: &[&str] = &["cmd", "interval", "retries", "start-period", "timeout"];

/// List flags whose element order carries no meaning to the runtime. Their values
/// are sorted before hashing so that reordering `ports` in a plan is a cosmetic
/// edit rather than a recreate — and therefore not downtime for a running service.
/// `security-opt` is deliberately absent: repeated options with the same key can
/// be last-one-wins, so order there can change behaviour.
const ORDER_INSENSITIVE_LISTS: &[&str] = &[
    "add-host",
    "cap-add",
    "cap-drop",
    "devices",
    "dns",
    "network-alias",
    "ports",
    "tmpfs",
    "volumes",
];

/// The value shape a parameter accepts. Checked up front so a mistyped value
/// fails loudly instead of being dropped by a `None` from a typed accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A string.
    Text,
    /// A string, or an integer that is stringified (`memory 2048`).
    Scalar,
    /// `#true` / `#false`.
    Flag,
    /// A `key { - "value" }` list block.
    List,
    /// A `key { name "value" }` block.
    Map,
    /// An integer.
    Count,
}

impl Shape {
    fn accepts(self, value: &ParamValue) -> bool {
        match self {
            Shape::Text => matches!(value, ParamValue::String(_)),
            Shape::Scalar => matches!(value, ParamValue::String(_) | ParamValue::Integer(_)),
            Shape::Flag => matches!(value, ParamValue::Bool(_)),
            Shape::List => matches!(value, ParamValue::List(_)),
            Shape::Map => matches!(value, ParamValue::Map(_)),
            Shape::Count => matches!(value, ParamValue::Integer(_)),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Shape::Text => "a string",
            Shape::Scalar => "a string or an integer",
            Shape::Flag => "a boolean (#true or #false)",
            Shape::List => "a list block, e.g. ports { - \"8080:80\" }",
            Shape::Map => "a block of key/value pairs",
            Shape::Count => "an integer",
        }
    }
}

/// The shape a known parameter accepts, or `None` if the parameter is unknown.
/// `success_codes` is absent on purpose: it accepts a string, an integer or a
/// list, and `parse_success_codes` already reports a bad value precisely.
fn shape_of(key: &str) -> Option<Shape> {
    if key == "success_codes" {
        return None;
    }
    if EQ_FLAGS.iter().any(|(k, _)| *k == key)
        || VALUE_FLAGS.iter().any(|(k, _)| *k == key)
        || key == "gpus"
    {
        return Some(Shape::Scalar);
    }
    if BOOL_FLAGS.iter().any(|(k, _)| *k == key) {
        return Some(Shape::Flag);
    }
    if LIST_FLAGS.iter().any(|(k, _)| *k == key) || key == "extra-args" {
        return Some(Shape::List);
    }
    if MAP_FLAGS.iter().any(|(k, _)| *k == key) || key == "healthcheck" {
        return Some(Shape::Map);
    }
    match key {
        "image" | "command" | "state" | "runtime" | "check" | "ready-cmd" | "wait" => {
            Some(Shape::Text)
        }
        "install-runtime" | "remove" => Some(Shape::Flag),
        "timeout" | "retries" | "delay" | "wait-timeout" | "wait-interval" => Some(Shape::Count),
        _ => None,
    }
}

/// A scalar parameter as text. Integers are accepted and stringified so
/// `memory 2048` behaves like `memory "2048"` rather than being silently dropped.
fn text_of(params: &ModuleParams, key: &str) -> Option<String> {
    match params.args.get(key)? {
        ParamValue::String(s) => Some(s.clone()),
        ParamValue::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

/// The runtimes glidesh knows how to drive. `runtime` reaches a shell command and
/// selects the package set for `install-runtime`, so an unrecognised value would
/// otherwise produce nonsense commands and install the wrong packages.
pub(super) const SUPPORTED_RUNTIMES: &[&str] = &["docker", "podman"];

/// Reject unknown parameters. A silently ignored `privledged` is exactly the kind
/// of failure that only shows up as strange runtime behaviour hours later.
pub(super) fn validate_params(params: &ModuleParams) -> Result<(), GlideshError> {
    let known = |key: &str| {
        GLIDESH_PARAMS.contains(&key)
            || SPECIAL_PARAMS.contains(&key)
            || EQ_FLAGS.iter().any(|(k, _)| *k == key)
            || VALUE_FLAGS.iter().any(|(k, _)| *k == key)
            || BOOL_FLAGS.iter().any(|(k, _)| *k == key)
            || LIST_FLAGS.iter().any(|(k, _)| *k == key)
            || MAP_FLAGS.iter().any(|(k, _)| *k == key)
    };

    let mut unknown: Vec<&str> = params
        .args
        .keys()
        .map(String::as_str)
        .filter(|k| !known(k))
        .collect();

    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: format!(
                "unknown parameter(s) for container '{}': {}",
                params.resource_name,
                unknown.join(", ")
            ),
        });
    }

    let mut mistyped: Vec<String> = params
        .args
        .iter()
        .filter_map(|(key, value)| {
            let shape = shape_of(key)?;
            (!shape.accepts(value)).then(|| format!("'{}' must be {}", key, shape.describe()))
        })
        .collect();
    if !mistyped.is_empty() {
        mistyped.sort();
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: format!(
                "container '{}': {}",
                params.resource_name,
                mistyped.join("; ")
            ),
        });
    }

    if let Some(runtime) = params.args.get("runtime") {
        let value = runtime.as_str().unwrap_or("");
        if !value.is_empty() && !SUPPORTED_RUNTIMES.contains(&value) {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "container '{}': unsupported runtime '{}' (expected one of: {})",
                    params.resource_name,
                    value,
                    SUPPORTED_RUNTIMES.join(", ")
                ),
            });
        }
    }

    Ok(())
}

/// The `run` invocation in two views, built in one pass from the same values.
pub(super) struct RunArgs {
    /// Every token following `run [-d] --name <name>`, in emission order.
    pub tokens: Vec<String>,
    /// The same content, canonicalised for hashing: values of order-insensitive
    /// list flags are sorted, so reordering them in a plan is not drift.
    canonical: Vec<String>,
}

impl RunArgs {
    fn push(&mut self, token: String) {
        self.canonical.push(token.clone());
        self.tokens.push(token);
    }

    /// Emit one `--flag value` pair per element, in plan order for the command
    /// and in sorted order for the hash when the flag is order-insensitive.
    fn push_list(&mut self, flag: &str, values: &[String], order_matters: bool) {
        for value in values {
            self.tokens.push(flag.to_string());
            self.tokens.push(shell_escape(value));
        }
        let mut canonical: Vec<&String> = values.iter().collect();
        if !order_matters {
            canonical.sort_unstable();
        }
        for value in canonical {
            self.canonical.push(flag.to_string());
            self.canonical.push(shell_escape(value));
        }
    }
}

/// Build every token that follows `run [-d] --name <name>`, in a fixed order so
/// the command — and the hash taken over it — is stable across runs.
pub(super) fn build_run_args(
    runtime: &str,
    params: &ModuleParams,
) -> Result<RunArgs, GlideshError> {
    let raw_image = text_of(params, "image")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GlideshError::Module {
            module: "container".to_string(),
            message: format!(
                "container '{}' requires an 'image' parameter",
                params.resource_name
            ),
        })?;

    let mut args = RunArgs {
        tokens: Vec::new(),
        canonical: Vec::new(),
    };

    for (key, flag) in EQ_FLAGS {
        if let Some(value) = text_of(params, key) {
            args.push(format!("{}={}", flag, shell_escape(&value)));
        }
    }

    for (key, flag) in VALUE_FLAGS {
        if let Some(value) = text_of(params, key) {
            args.push(flag.to_string());
            args.push(shell_escape(&value));
        }
    }

    for (key, flag) in BOOL_FLAGS {
        if params.args.get(*key).and_then(|v| v.as_bool()) == Some(true) {
            args.push(flag.to_string());
        }
    }

    if let Some(gpus) = text_of(params, "gpus") {
        for token in gpu_args(runtime, &gpus) {
            args.push(token);
        }
    }

    for (key, flag) in LIST_FLAGS {
        if let Some(values) = params.args.get(*key).and_then(|v| v.as_list()) {
            args.push_list(flag, values, !ORDER_INSENSITIVE_LISTS.contains(key));
        }
    }

    for (key, flag) in MAP_FLAGS {
        if let Some(map) = params.args.get(*key).and_then(|v| v.as_map()) {
            let mut pairs: Vec<_> = map.iter().collect();
            pairs.sort_by_key(|(k, _)| *k);
            for (k, v) in pairs {
                args.push(flag.to_string());
                args.push(shell_escape(&format!("{}={}", k, v)));
            }
        }
    }

    for token in healthcheck_args(params)? {
        args.push(token);
    }

    // Escape hatch: forwarded verbatim, unquoted, for flags glidesh has no
    // first-class parameter for. Order is preserved everywhere, including in the
    // hash — these are raw flags whose order can matter.
    if let Some(extra) = params.args.get("extra-args").and_then(|v| v.as_list()) {
        for token in extra {
            args.push(token.clone());
        }
    }

    args.push(shell_escape(&qualify_image(&raw_image, runtime)));

    // The command keeps its own quoting so shell metacharacters written in the
    // plan (`nginx -g 'daemon off;'`) reach the container intact.
    if let Some(command) = text_of(params, "command") {
        if !command.is_empty() {
            args.push(command);
        }
    }

    Ok(args)
}

/// `--gpus` is Docker-only; Podman exposes the same devices through CDI.
fn gpu_args(runtime: &str, value: &str) -> Vec<String> {
    if runtime != "podman" {
        return vec!["--gpus".to_string(), shell_escape(value)];
    }
    let device = if value.contains('=') {
        value.to_string()
    } else {
        format!("nvidia.com/gpu={}", value)
    };
    vec!["--device".to_string(), shell_escape(&device)]
}

fn healthcheck_args(params: &ModuleParams) -> Result<Vec<String>, GlideshError> {
    let Some(hc) = params.args.get("healthcheck") else {
        return Ok(Vec::new());
    };
    let Some(map) = hc.as_map() else {
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: "healthcheck must be a block of key/value pairs".to_string(),
        });
    };

    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !HEALTHCHECK_KEYS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: format!("unknown healthcheck key(s): {}", unknown.join(", ")),
        });
    }

    let cmd = map.get("cmd").map(String::as_str).unwrap_or("").trim();
    if cmd.is_empty() {
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: "healthcheck requires a 'cmd'".to_string(),
        });
    }
    if cmd.eq_ignore_ascii_case("none") {
        return Ok(vec!["--no-healthcheck".to_string()]);
    }

    let mut args = vec!["--health-cmd".to_string(), shell_escape(cmd)];
    for (key, flag) in [
        ("interval", "--health-interval"),
        ("timeout", "--health-timeout"),
        ("start-period", "--health-start-period"),
    ] {
        if let Some(value) = map.get(key) {
            args.push(flag.to_string());
            args.push(shell_escape(&as_duration(value)));
        }
    }
    if let Some(retries) = map.get("retries") {
        args.push("--health-retries".to_string());
        args.push(shell_escape(retries));
    }
    Ok(args)
}

/// A bare number in a healthcheck block means seconds; anything with a unit
/// suffix (`30s`, `2m`) is passed through untouched.
fn as_duration(value: &str) -> String {
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        format!("{}s", trimmed)
    } else {
        trimmed.to_string()
    }
}

fn hash_args(args: &[String]) -> String {
    let mut hasher = Sha256::new();
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Hash of the container's full desired spec, stored as a label so a later run
/// can tell whether the plan has changed under a live container.
pub(super) fn spec_hash(runtime: &str, params: &ModuleParams) -> Result<String, GlideshError> {
    Ok(hash_args(&build_run_args(runtime, params)?.canonical))
}

/// `<runtime> run -d` for a long-lived container, labelled with its spec hash.
pub(super) fn build_run_command(
    runtime: &str,
    container_name: &str,
    params: &ModuleParams,
) -> Result<String, GlideshError> {
    let args = build_run_args(runtime, params)?;
    let hash = hash_args(&args.canonical);
    Ok(format!(
        "{} run -d --name {} --label {}={} {}",
        runtime,
        shell_escape(container_name),
        PARAM_HASH_LABEL,
        hash,
        args.tokens.join(" ")
    ))
}

/// `<runtime> run` in the foreground for a one-shot job. No spec label: nothing
/// survives the run to compare against.
pub(super) fn build_run_once_command(
    runtime: &str,
    container_name: &str,
    params: &ModuleParams,
) -> Result<String, GlideshError> {
    if params.args.contains_key("restart") {
        return Err(GlideshError::Module {
            module: "container".to_string(),
            message: format!(
                "container '{}': 'restart' has no meaning with state \"run-once\"",
                container_name
            ),
        });
    }
    let remove = params
        .args
        .get("remove")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let args = build_run_args(runtime, params)?;
    Ok(format!(
        "{} run{} --name {} {}",
        runtime,
        if remove { " --rm" } else { "" },
        shell_escape(container_name),
        args.tokens.join(" ")
    ))
}

/// Qualify a short image name for Podman which doesn't default to Docker Hub.
/// If the image has no registry prefix (no `.` or `localhost` before the first `/`),
/// prepend `docker.io/`. For Docker this is a no-op since Docker already defaults
/// to Docker Hub, but the explicit prefix doesn't hurt.
pub(super) fn qualify_image(image: &str, runtime: &str) -> String {
    if runtime != "podman" {
        return image.to_string();
    }
    if let Some(slash_pos) = image.find('/') {
        let prefix = &image[..slash_pos];
        if prefix.contains('.') || prefix == "localhost" {
            return image.to_string();
        }
    }
    format!("docker.io/{}", image)
}

/// Returns true for Docker/Podman built-in network modes that should not be auto-created.
pub(super) fn is_builtin_network(name: &str) -> bool {
    matches!(name, "host" | "bridge" | "none" | "default")
        || name.starts_with("container:")
        || name.starts_with("ns:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_params(args: Vec<(&str, ParamValue)>) -> ModuleParams {
        ModuleParams {
            resource_name: "testcontainer".to_string(),
            args: args.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> ParamValue {
        ParamValue::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        )
    }

    #[test]
    fn basic_run_command() {
        let params = make_params(vec![("image", ParamValue::String("nginx:latest".into()))]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(
            cmd.starts_with("docker run -d --name 'testcontainer' --label sh.glide.param-hash=")
        );
        assert!(cmd.ends_with("'nginx:latest'"));
    }

    #[test]
    fn command_is_appended_unquoted() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            (
                "command",
                ParamValue::String("nginx -g 'daemon off;'".into()),
            ),
        ]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.ends_with("'nginx:latest' nginx -g 'daemon off;'"));
    }

    #[test]
    fn missing_image_is_an_error() {
        let params = make_params(vec![]);
        assert!(build_run_command("docker", "testcontainer", &params).is_err());
    }

    #[test]
    fn namespace_and_privilege_flags() {
        let params = make_params(vec![
            ("image", ParamValue::String("vllm:latest".into())),
            ("ipc", ParamValue::String("host".into())),
            ("pid", ParamValue::String("host".into())),
            ("privileged", ParamValue::Bool(true)),
            ("shm-size", ParamValue::String("16g".into())),
        ]);
        let cmd = build_run_command("docker", "vllm", &params).unwrap();
        assert!(cmd.contains("--ipc='host'"));
        assert!(cmd.contains("--pid='host'"));
        assert!(cmd.contains("--privileged"));
        assert!(cmd.contains("--shm-size '16g'"));
    }

    #[test]
    fn privileged_false_emits_nothing() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx".into())),
            ("privileged", ParamValue::Bool(false)),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(!cmd.contains("--privileged"));
    }

    #[test]
    fn entrypoint_is_escaped() {
        let params = make_params(vec![
            ("image", ParamValue::String("py:3.12".into())),
            ("entrypoint", ParamValue::String("/bin/bash".into())),
            ("command", ParamValue::String("-c 'echo hi'".into())),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(cmd.contains("--entrypoint '/bin/bash'"));
        assert!(cmd.ends_with("'py:3.12' -c 'echo hi'"));
    }

    #[test]
    fn gpus_docker_vs_podman() {
        let params = make_params(vec![
            ("image", ParamValue::String("cuda".into())),
            ("gpus", ParamValue::String("all".into())),
        ]);
        assert!(
            build_run_command("docker", "x", &params)
                .unwrap()
                .contains("--gpus 'all'")
        );
        assert!(
            build_run_command("podman", "x", &params)
                .unwrap()
                .contains("--device 'nvidia.com/gpu=all'")
        );
    }

    #[test]
    fn gpus_podman_passes_cdi_spec_through() {
        let params = make_params(vec![
            ("image", ParamValue::String("cuda".into())),
            ("gpus", ParamValue::String("nvidia.com/gpu=0".into())),
        ]);
        let cmd = build_run_command("podman", "x", &params).unwrap();
        assert!(cmd.contains("--device 'nvidia.com/gpu=0'"));
    }

    #[test]
    fn healthcheck_flags() {
        let params = make_params(vec![
            ("image", ParamValue::String("vllm".into())),
            (
                "healthcheck",
                map(&[
                    ("cmd", "curl -sf http://localhost:8000/health"),
                    ("interval", "10"),
                    ("timeout", "5s"),
                    ("retries", "3"),
                    ("start-period", "300"),
                ]),
            ),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(cmd.contains("--health-cmd 'curl -sf http://localhost:8000/health'"));
        assert!(cmd.contains("--health-interval '10s'"));
        assert!(cmd.contains("--health-timeout '5s'"));
        assert!(cmd.contains("--health-start-period '300s'"));
        assert!(cmd.contains("--health-retries '3'"));
    }

    #[test]
    fn healthcheck_none_disables() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("healthcheck", map(&[("cmd", "NONE")])),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(cmd.contains("--no-healthcheck"));
    }

    #[test]
    fn healthcheck_rejects_unknown_key() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("healthcheck", map(&[("cmd", "true"), ("interal", "10")])),
        ]);
        assert!(build_run_command("docker", "x", &params).is_err());
    }

    #[test]
    fn extra_args_are_verbatim() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            (
                "extra-args",
                ParamValue::List(vec!["--gpus all".into(), "--ulimit memlock=-1".into()]),
            ),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(cmd.contains("--gpus all --ulimit memlock=-1 'x'"));
    }

    #[test]
    fn env_and_labels_are_sorted_for_stability() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            (
                "environment",
                map(&[("ZED", "1"), ("ALPHA", "2"), ("MID", "3")]),
            ),
        ]);
        let first = build_run_command("docker", "x", &params).unwrap();
        for _ in 0..20 {
            assert_eq!(build_run_command("docker", "x", &params).unwrap(), first);
        }
        let alpha = first.find("ALPHA").unwrap();
        let mid = first.find("MID").unwrap();
        let zed = first.find("ZED").unwrap();
        assert!(alpha < mid && mid < zed);
    }

    #[test]
    fn spec_hash_covers_every_forwarded_flag() {
        let base = make_params(vec![("image", ParamValue::String("x".into()))]);
        let baseline = spec_hash("docker", &base).unwrap();

        for (key, value) in [
            ("ipc", ParamValue::String("host".into())),
            ("privileged", ParamValue::Bool(true)),
            ("entrypoint", ParamValue::String("/bin/sh".into())),
            ("gpus", ParamValue::String("all".into())),
            ("shm-size", ParamValue::String("8g".into())),
            ("devices", ParamValue::List(vec!["/dev/fuse".into()])),
            ("cap-add", ParamValue::List(vec!["SYS_PTRACE".into()])),
            ("ulimits", map(&[("memlock", "-1")])),
            ("extra-args", ParamValue::List(vec!["--init".into()])),
        ] {
            let mut params = base.clone();
            params.args.insert(key.to_string(), value);
            assert_ne!(
                spec_hash("docker", &params).unwrap(),
                baseline,
                "{key} does not affect the spec hash"
            );
        }
    }

    #[test]
    fn spec_hash_ignores_glidesh_side_params() {
        let base = make_params(vec![("image", ParamValue::String("x".into()))]);
        let baseline = spec_hash("docker", &base).unwrap();
        for (key, value) in [
            ("wait", ParamValue::String("healthy".into())),
            ("wait-timeout", ParamValue::Integer(900)),
            ("ready-cmd", ParamValue::String("curl -sf localhost".into())),
            ("runtime", ParamValue::String("docker".into())),
            ("state", ParamValue::String("running".into())),
        ] {
            let mut params = base.clone();
            params.args.insert(key.to_string(), value);
            assert_eq!(spec_hash("docker", &params).unwrap(), baseline);
        }
    }

    #[test]
    fn run_once_uses_rm_and_no_detach() {
        let params = make_params(vec![
            ("image", ParamValue::String("hf:latest".into())),
            ("entrypoint", ParamValue::String("/bin/bash".into())),
        ]);
        let cmd = build_run_once_command("docker", "hf-download", &params).unwrap();
        assert!(cmd.starts_with("docker run --rm --name 'hf-download' "));
        assert!(!cmd.contains(" -d "));
        assert!(!cmd.contains(PARAM_HASH_LABEL));
    }

    #[test]
    fn run_once_keeps_container_when_remove_false() {
        let params = make_params(vec![
            ("image", ParamValue::String("hf:latest".into())),
            ("remove", ParamValue::Bool(false)),
        ]);
        let cmd = build_run_once_command("docker", "job", &params).unwrap();
        assert!(cmd.starts_with("docker run --name 'job' "));
    }

    #[test]
    fn run_once_rejects_restart_policy() {
        let params = make_params(vec![
            ("image", ParamValue::String("hf:latest".into())),
            ("restart", ParamValue::String("always".into())),
        ]);
        assert!(build_run_once_command("docker", "job", &params).is_err());
    }

    #[test]
    fn validate_rejects_typos() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("privledged", ParamValue::Bool(true)),
        ]);
        let err = validate_params(&params).unwrap_err().to_string();
        assert!(err.contains("privledged"));
    }

    fn every_param() -> impl Iterator<Item = &'static &'static str> {
        GLIDESH_PARAMS
            .iter()
            .chain(SPECIAL_PARAMS)
            .chain(EQ_FLAGS.iter().map(|(k, _)| k))
            .chain(VALUE_FLAGS.iter().map(|(k, _)| k))
            .chain(BOOL_FLAGS.iter().map(|(k, _)| k))
            .chain(LIST_FLAGS.iter().map(|(k, _)| k))
            .chain(MAP_FLAGS.iter().map(|(k, _)| k))
    }

    fn sample_for(key: &str) -> ParamValue {
        if key == "runtime" {
            return ParamValue::String("docker".into());
        }
        match shape_of(key) {
            Some(Shape::Flag) => ParamValue::Bool(true),
            Some(Shape::List) => ParamValue::List(vec!["v".into()]),
            Some(Shape::Map) => map(&[("cmd", "true")]),
            Some(Shape::Count) => ParamValue::Integer(1),
            _ => ParamValue::String("v".into()),
        }
    }

    #[test]
    fn validate_accepts_every_documented_param() {
        for key in every_param() {
            let params = make_params(vec![(key, sample_for(key))]);
            assert!(validate_params(&params).is_ok(), "{key} rejected");
        }
    }

    /// Every documented parameter must declare a shape, or a mistyped value for
    /// it would still be dropped in silence.
    #[test]
    fn every_param_has_a_declared_shape() {
        for key in every_param() {
            assert!(
                shape_of(key).is_some() || *key == "success_codes",
                "{key} has no declared shape"
            );
        }
    }

    #[test]
    fn validate_rejects_mistyped_values() {
        for (key, value, expected) in [
            // The dangerous one: a string where a list block belongs would
            // otherwise produce a container with no published ports.
            ("ports", ParamValue::String("8080:80".into()), "list block"),
            ("volumes", ParamValue::String("/a:/b".into()), "list block"),
            ("privileged", ParamValue::String("yes".into()), "boolean"),
            ("runtime", ParamValue::Bool(true), "must be a string"),
            ("wait", ParamValue::Integer(1), "must be a string"),
            ("wait-timeout", ParamValue::String("900".into()), "integer"),
            ("environment", ParamValue::String("A=1".into()), "key/value"),
            (
                "healthcheck",
                ParamValue::String("true".into()),
                "key/value",
            ),
            (
                "install-runtime",
                ParamValue::String("true".into()),
                "boolean",
            ),
        ] {
            let params = make_params(vec![
                ("image", ParamValue::String("x".into())),
                (key, value),
            ]);
            let err = validate_params(&params).unwrap_err().to_string();
            assert!(err.contains(key), "{key}: {err}");
            assert!(err.contains(expected), "{key}: {err}");
        }
    }

    #[test]
    fn scalar_flags_accept_integers() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("memory", ParamValue::Integer(2048)),
            ("shm-size", ParamValue::Integer(16)),
        ]);
        validate_params(&params).unwrap();
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(cmd.contains("--memory '2048'"), "{cmd}");
        assert!(cmd.contains("--shm-size '16'"), "{cmd}");
    }

    /// Reordering a list whose order the runtime ignores is a cosmetic edit, not
    /// drift: recreating a live container over it would be needless downtime.
    #[test]
    fn reordering_order_insensitive_lists_is_not_drift() {
        for key in ORDER_INSENSITIVE_LISTS {
            let forward = make_params(vec![
                ("image", ParamValue::String("x".into())),
                (
                    key,
                    ParamValue::List(vec!["a".into(), "b".into(), "c".into()]),
                ),
            ]);
            let shuffled = make_params(vec![
                ("image", ParamValue::String("x".into())),
                (
                    key,
                    ParamValue::List(vec!["c".into(), "a".into(), "b".into()]),
                ),
            ]);
            assert_eq!(
                spec_hash("docker", &forward).unwrap(),
                spec_hash("docker", &shuffled).unwrap(),
                "{key} reorder was treated as drift"
            );
        }
    }

    #[test]
    fn the_command_keeps_the_plan_order_it_was_written_in() {
        let params = make_params(vec![
            ("image", ParamValue::String("x".into())),
            (
                "ports",
                ParamValue::List(vec!["9090:90".into(), "8080:80".into()]),
            ),
        ]);
        let cmd = build_run_command("docker", "x", &params).unwrap();
        assert!(
            cmd.find("9090:90").unwrap() < cmd.find("8080:80").unwrap(),
            "sorting is for the hash only: {cmd}"
        );
    }

    #[test]
    fn changing_a_list_value_is_still_drift() {
        let before = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("ports", ParamValue::List(vec!["8080:80".into()])),
        ]);
        let after = make_params(vec![
            ("image", ParamValue::String("x".into())),
            ("ports", ParamValue::List(vec!["9090:80".into()])),
        ]);
        assert_ne!(
            spec_hash("docker", &before).unwrap(),
            spec_hash("docker", &after).unwrap()
        );
    }

    /// `extra-args` and `security-opt` are raw enough that order can change
    /// behaviour, so a reorder must still count as drift.
    #[test]
    fn reordering_order_sensitive_lists_is_drift() {
        for (key, a, b) in [
            ("extra-args", "--cpuset-cpus=0", "--cpuset-cpus=1"),
            ("security-opt", "seccomp=unconfined", "label=disable"),
        ] {
            let forward = make_params(vec![
                ("image", ParamValue::String("x".into())),
                (key, ParamValue::List(vec![a.into(), b.into()])),
            ]);
            let reversed = make_params(vec![
                ("image", ParamValue::String("x".into())),
                (key, ParamValue::List(vec![b.into(), a.into()])),
            ]);
            assert_ne!(
                spec_hash("docker", &forward).unwrap(),
                spec_hash("docker", &reversed).unwrap(),
                "{key} reorder must be drift"
            );
        }
    }

    #[test]
    fn validate_rejects_unsupported_runtime() {
        for value in ["containerd", "Docker", "docker; rm -rf /"] {
            let params = make_params(vec![
                ("image", ParamValue::String("x".into())),
                ("runtime", ParamValue::String(value.into())),
            ]);
            let err = validate_params(&params).unwrap_err().to_string();
            assert!(err.contains("unsupported runtime"), "{value}: {err}");
        }
    }

    #[test]
    fn validate_accepts_supported_runtimes() {
        for value in SUPPORTED_RUNTIMES {
            let params = make_params(vec![
                ("image", ParamValue::String("x".into())),
                ("runtime", ParamValue::String((*value).into())),
            ]);
            assert!(validate_params(&params).is_ok(), "{value} rejected");
        }
    }

    /// The examples are documentation people copy. If validation would reject
    /// them, the docs are wrong or the validation is.
    #[test]
    fn shipped_examples_pass_validation() {
        for (name, source) in [
            (
                "gpu-inference",
                include_str!("../../../examples/gpu-inference/plan.kdl"),
            ),
            (
                "container-app",
                include_str!("../../../examples/container-app/plan.kdl"),
            ),
            (
                "hello-echo",
                include_str!("../../../examples/hello-echo/plan.kdl"),
            ),
        ] {
            let plan = crate::config::plan::parse_plan(source)
                .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
            let mut seen = 0;
            for step in plan.steps() {
                for task in &step.tasks {
                    if task.module != "container" {
                        continue;
                    }
                    seen += 1;
                    let params = ModuleParams {
                        resource_name: task.resource.clone(),
                        args: task.args.clone(),
                    };
                    validate_params(&params)
                        .unwrap_or_else(|e| panic!("{name}/{}: {e}", task.resource));
                }
            }
            assert!(seen > 0, "{name} has no container tasks to check");
        }
    }

    #[test]
    fn builtin_networks() {
        assert!(is_builtin_network("host"));
        assert!(is_builtin_network("bridge"));
        assert!(is_builtin_network("none"));
        assert!(is_builtin_network("default"));
        assert!(is_builtin_network("container:other"));
        assert!(!is_builtin_network("app-network"));
    }

    /// End-to-end from KDL: block syntax must survive parsing into the shapes
    /// `build_run_args` expects (bool switches, maps, and lists).
    #[test]
    fn kdl_plan_produces_the_expected_run_command() {
        let plan = crate::config::plan::parse_plan(
            r#"
plan "p" {
    step "s" {
        container "vllm" {
            image "vllm/vllm-openai:latest"
            ipc "host"
            privileged #true
            gpus "all"
            shm-size "16g"
            entrypoint "/bin/bash"
            restart "always"
            ports {
                - "8000:8000"
            }
            ulimits {
                memlock "-1"
            }
            healthcheck {
                cmd "curl -sf http://localhost:8000/health"
                interval 10
                start-period 120
            }
            wait "healthy"
            wait-timeout 1800
        }
    }
}
"#,
        )
        .unwrap();

        let task = &plan.steps()[0].tasks[0];
        let params = ModuleParams {
            resource_name: task.resource.clone(),
            args: task.args.clone(),
        };
        validate_params(&params).unwrap();

        let cmd = build_run_command("docker", &params.resource_name, &params).unwrap();
        assert!(cmd.contains("--ipc='host'"), "{cmd}");
        assert!(cmd.contains("--privileged"), "{cmd}");
        assert!(cmd.contains("--gpus 'all'"), "{cmd}");
        assert!(cmd.contains("--shm-size '16g'"), "{cmd}");
        assert!(cmd.contains("--entrypoint '/bin/bash'"), "{cmd}");
        assert!(cmd.contains("--restart='always'"), "{cmd}");
        assert!(cmd.contains("-p '8000:8000'"), "{cmd}");
        assert!(cmd.contains("--ulimit 'memlock=-1'"), "{cmd}");
        assert!(cmd.contains("--health-interval '10s'"), "{cmd}");
        assert!(cmd.contains("--health-start-period '120s'"), "{cmd}");
        // Readiness is glidesh-side and must not leak into the runtime command.
        assert!(!cmd.contains("--wait"), "{cmd}");
        assert!(cmd.ends_with("'vllm/vllm-openai:latest'"), "{cmd}");
    }

    #[test]
    fn qualify_image_rules() {
        assert_eq!(qualify_image("nginx:latest", "docker"), "nginx:latest");
        assert_eq!(
            qualify_image("nginx:latest", "podman"),
            "docker.io/nginx:latest"
        );
        assert_eq!(
            qualify_image("ghcr.io/org/app:v1", "podman"),
            "ghcr.io/org/app:v1"
        );
    }
}
