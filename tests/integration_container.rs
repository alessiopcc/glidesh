//! Container module integration tests.
//!
//! Docker-in-Docker inside the test container is too heavy and too flaky to be
//! worth it, so these drive the module over a real SSH session against a
//! `docker` CLI stand-in (`tests/fake-docker.sh`) that models the container
//! lifecycle and records every invocation. That covers what actually breaks in
//! this module — the check/apply state machine, drift detection, removal
//! handling, readiness gating, and the exact flags emitted — while leaving the
//! runtime's own behaviour to the runtime.

mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::container::ContainerModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use glidesh::ssh::SshSession;
use std::collections::HashMap;
use std::time::Duration;

const FAKE_DOCKER: &str = include_str!("fake-docker.sh");
const STATE_ROOT: &str = "/var/lib/fakedocker";

/// Install the stand-in runtime. Must run before `detect_os`, which is what
/// decides whether the module sees a runtime at all.
async fn install_fake_docker(ssh: &SshSession) {
    let out = ssh
        .exec(&format!(
            "cat > /usr/local/bin/docker <<'GLIDESH_FAKE_DOCKER_EOF'\n{}\nGLIDESH_FAKE_DOCKER_EOF\nchmod +x /usr/local/bin/docker && which docker",
            FAKE_DOCKER
        ))
        .await
        .expect("failed to install fake docker");
    assert_eq!(
        out.exit_code, 0,
        "installing fake docker failed: {}",
        out.stderr
    );
}

async fn read_state(ssh: &SshSession, container: &str, file: &str) -> String {
    let out = ssh
        .exec(&format!(
            "cat {}/c/{}/{} 2>/dev/null",
            STATE_ROOT, container, file
        ))
        .await
        .expect("failed to read fake docker state");
    out.stdout.trim().to_string()
}

async fn write_state(ssh: &SshSession, container: &str, file: &str, value: &str) {
    let out = ssh
        .exec(&format!(
            "printf '%s' '{}' > {}/c/{}/{}",
            value, STATE_ROOT, container, file
        ))
        .await
        .expect("failed to write fake docker state");
    assert_eq!(out.exit_code, 0, "write_state failed: {}", out.stderr);
}

async fn container_exists(ssh: &SshSession, container: &str) -> bool {
    ssh.exec(&format!("test -d {}/c/{}", STATE_ROOT, container))
        .await
        .expect("failed to stat fake docker state")
        .exit_code
        == 0
}

fn params(name: &str, args: &[(&str, ParamValue)]) -> ModuleParams {
    ModuleParams {
        resource_name: name.to_string(),
        args: args
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    }
}

fn s(value: &str) -> ParamValue {
    ParamValue::String(value.to_string())
}

/// Create → idempotent → externally stopped → started in place → config change
/// → recreated. The generation counter proves whether the container survived.
#[tokio::test]
async fn test_container_lifecycle_reuses_before_recreating() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params(
        "webapp",
        &[("image", s("nginx:alpine")), ("restart", s("always"))],
    );

    assert!(matches!(
        ContainerModule.check(&ctx, &spec).await.unwrap(),
        ModuleStatus::Pending { .. }
    ));

    let result = ContainerModule.apply(&ctx, &spec).await.unwrap();
    assert!(result.changed);
    assert_eq!(read_state(&ssh, "webapp", "status").await, "running");
    let first_gen = read_state(&ssh, "webapp", "generation").await;

    assert!(
        matches!(
            ContainerModule.check(&ctx, &spec).await.unwrap(),
            ModuleStatus::Satisfied
        ),
        "an unchanged running container must be satisfied"
    );

    // Something stopped it out of band.
    write_state(&ssh, "webapp", "status", "exited").await;
    match ContainerModule.check(&ctx, &spec).await.unwrap() {
        ModuleStatus::Pending { plan } => assert!(
            plan.contains("Start container webapp"),
            "expected a start plan, got: {plan}"
        ),
        other => panic!("expected Pending, got {other:?}"),
    }

    let result = ContainerModule.apply(&ctx, &spec).await.unwrap();
    assert!(result.changed);
    assert_eq!(read_state(&ssh, "webapp", "status").await, "running");
    assert_eq!(
        read_state(&ssh, "webapp", "generation").await,
        first_gen,
        "a stopped container whose spec still matches must be started, not recreated"
    );

    // A changed spec must recreate.
    let changed_spec = params(
        "webapp",
        &[
            ("image", s("nginx:alpine")),
            ("restart", s("always")),
            ("ipc", s("host")),
        ],
    );
    match ContainerModule.check(&ctx, &changed_spec).await.unwrap() {
        ModuleStatus::Pending { plan } => assert!(
            plan.contains("configuration changed"),
            "expected a drift plan, got: {plan}"
        ),
        other => panic!("expected Pending, got {other:?}"),
    }
    ContainerModule.apply(&ctx, &changed_spec).await.unwrap();
    assert_ne!(
        read_state(&ssh, "webapp", "generation").await,
        first_gen,
        "a changed spec must produce a new container"
    );
}

/// A container that cannot be removed must surface that, not the runtime's
/// downstream "name is already in use" from the follow-up `run`.
#[tokio::test]
async fn test_stuck_container_reports_the_removal_failure() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params("stuck", &[("image", s("nginx:alpine"))]);
    ContainerModule.apply(&ctx, &spec).await.unwrap();

    ssh.exec(&format!("touch {}/c/stuck/undeletable", STATE_ROOT))
        .await
        .unwrap();

    let changed = params("stuck", &[("image", s("nginx:1.27")), ("ipc", s("host"))]);
    let err = ContainerModule
        .apply(&ctx, &changed)
        .await
        .expect_err("recreating an unremovable container must fail");
    let message = err.to_string();
    assert!(
        message.contains("could not remove the existing container 'stuck'"),
        "expected the removal failure to be surfaced, got: {message}"
    );
    assert!(
        !message.contains("already in use"),
        "the opaque name clash must not be what the user sees: {message}"
    );
}

/// Every new flag must actually reach the runtime, correctly quoted.
#[tokio::test]
async fn test_run_command_carries_gpu_and_privilege_flags() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params(
        "vllm",
        &[
            ("image", s("vllm/vllm-openai:latest")),
            ("ipc", s("host")),
            ("privileged", ParamValue::Bool(true)),
            ("gpus", s("all")),
            ("shm-size", s("16g")),
            ("entrypoint", s("/bin/bash")),
            (
                "ulimits",
                ParamValue::Map(HashMap::from([("memlock".to_string(), "-1".to_string())])),
            ),
            (
                "devices",
                ParamValue::List(vec!["/dev/infiniband:/dev/infiniband".to_string()]),
            ),
            (
                "extra-args",
                ParamValue::List(vec!["--cgroup-parent=inference".to_string()]),
            ),
            ("command", s("--model /models/GLM-4.6")),
        ],
    );

    ContainerModule.apply(&ctx, &spec).await.unwrap();
    let argv = read_state(&ssh, "vllm", "argv").await;

    for expected in [
        "--ipc=host",
        "--privileged",
        "--gpus all",
        "--shm-size 16g",
        "--entrypoint /bin/bash",
        "--ulimit memlock=-1",
        "--device /dev/infiniband:/dev/infiniband",
        "--cgroup-parent=inference",
        "vllm/vllm-openai:latest --model /models/GLM-4.6",
    ] {
        assert!(
            argv.contains(expected),
            "run arguments missing '{expected}': {argv}"
        );
    }
}

/// `wait "healthy"` must block until the runtime reports healthy, and fail with
/// the container's log tail when it never does.
#[tokio::test]
async fn test_wait_healthy_blocks_then_times_out() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let flipper = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params(
        "svc",
        &[
            ("image", s("nginx:alpine")),
            (
                "healthcheck",
                ParamValue::Map(HashMap::from([
                    ("cmd".to_string(), "curl -sf localhost".to_string()),
                    ("interval".to_string(), "1".to_string()),
                ])),
            ),
            ("wait", s("healthy")),
            ("wait-timeout", ParamValue::Integer(30)),
            ("wait-interval", ParamValue::Integer(1)),
        ],
    );

    // The container comes up `starting`; flip it healthy while apply is polling.
    let flip = async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        flipper
            .exec(&format!("printf healthy > {}/c/svc/health", STATE_ROOT))
            .await
            .expect("failed to flip health");
    };
    let (applied, ()) = tokio::join!(ContainerModule.apply(&ctx, &spec), flip);
    applied.expect("apply must succeed once the container reports healthy");
    assert!(matches!(
        ContainerModule.check(&ctx, &spec).await.unwrap(),
        ModuleStatus::Satisfied
    ));

    // Back to starting: check must gate, and a bounded wait must fail loudly.
    write_state(&ssh, "svc", "health", "starting").await;
    match ContainerModule.check(&ctx, &spec).await.unwrap() {
        ModuleStatus::Pending { plan } => assert!(
            plan.contains("become healthy"),
            "expected a readiness plan, got: {plan}"
        ),
        other => panic!("expected Pending, got {other:?}"),
    }

    let impatient = params(
        "svc",
        &[
            ("image", s("nginx:alpine")),
            (
                "healthcheck",
                ParamValue::Map(HashMap::from([
                    ("cmd".to_string(), "curl -sf localhost".to_string()),
                    ("interval".to_string(), "1".to_string()),
                ])),
            ),
            ("wait", s("healthy")),
            ("wait-timeout", ParamValue::Integer(3)),
            ("wait-interval", ParamValue::Integer(1)),
        ],
    );
    let err = ContainerModule
        .apply(&ctx, &impatient)
        .await
        .expect_err("a container that never becomes healthy must fail");
    let message = err.to_string();
    assert!(
        message.contains("never became ready") && message.contains("timed out"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("boot line one"),
        "the failure must carry the container log tail: {message}"
    );
}

/// `ready-cmd` probes from the host, for images with no shell of their own.
#[tokio::test]
async fn test_ready_cmd_gates_the_step() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params(
        "probe",
        &[
            ("image", s("nginx:alpine")),
            ("ready-cmd", s("test -f /tmp/probe-ready")),
            ("wait-timeout", ParamValue::Integer(3)),
            ("wait-interval", ParamValue::Integer(1)),
        ],
    );

    let err = ContainerModule
        .apply(&ctx, &spec)
        .await
        .expect_err("apply must fail while the probe is failing");
    assert!(
        err.to_string().contains("never became ready"),
        "unexpected error: {err}"
    );

    ssh.exec("touch /tmp/probe-ready").await.unwrap();
    ContainerModule
        .apply(&ctx, &spec)
        .await
        .expect("apply must succeed once the probe passes");
    assert!(matches!(
        ContainerModule.check(&ctx, &spec).await.unwrap(),
        ModuleStatus::Satisfied
    ));
}

/// One-shot jobs: skipped by their guard, retried on failure, and never leaving
/// a container behind to collide with the next attempt.
#[tokio::test]
async fn test_run_once_guard_and_retries() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let job = params(
        "hf-download",
        &[
            ("state", s("run-once")),
            ("image", s("python:3.12-slim")),
            ("entrypoint", s("/bin/bash")),
            ("check", s("test -f /tmp/weights-done")),
            ("retries", ParamValue::Integer(2)),
            (
                "environment",
                ParamValue::Map(HashMap::from([(
                    "FAKE_EXIT_ONCE".to_string(),
                    "7".to_string(),
                )])),
            ),
        ],
    );

    assert!(
        matches!(
            ContainerModule.check(&ctx, &job).await.unwrap(),
            ModuleStatus::Pending { .. }
        ),
        "the job must run while its guard is unsatisfied"
    );

    // First attempt exits 7, the retry succeeds.
    let result = ContainerModule.apply(&ctx, &job).await.unwrap();
    assert!(result.changed);
    assert!(
        !container_exists(&ssh, "hf-download").await,
        "a one-shot job must not leave a container behind"
    );

    ssh.exec("touch /tmp/weights-done").await.unwrap();
    assert!(
        matches!(
            ContainerModule.check(&ctx, &job).await.unwrap(),
            ModuleStatus::Satisfied
        ),
        "a satisfied guard must skip the job"
    );

    // A job that keeps failing surfaces the exit code it kept returning.
    let failing = params(
        "always-fails",
        &[
            ("state", s("run-once")),
            ("image", s("python:3.12-slim")),
            (
                "environment",
                ParamValue::Map(HashMap::from([("FAKE_EXIT".to_string(), "3".to_string())])),
            ),
        ],
    );
    let err = ContainerModule
        .apply(&ctx, &failing)
        .await
        .expect_err("a failing job must fail the task");
    assert!(
        err.to_string().contains("exit code 3"),
        "unexpected error: {err}"
    );

    // ...unless that code is declared successful.
    let tolerated = params(
        "tolerated",
        &[
            ("state", s("run-once")),
            ("image", s("python:3.12-slim")),
            ("success_codes", s("0,3")),
            (
                "environment",
                ParamValue::Map(HashMap::from([("FAKE_EXIT".to_string(), "3".to_string())])),
            ),
        ],
    );
    ContainerModule.apply(&ctx, &tolerated).await.unwrap();
}

/// `state "absent"` removes the container and is then satisfied.
#[tokio::test]
async fn test_absent_removes_container() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let running = params("doomed", &[("image", s("nginx:alpine"))]);
    ContainerModule.apply(&ctx, &running).await.unwrap();
    assert!(container_exists(&ssh, "doomed").await);

    let absent = params("doomed", &[("state", s("absent"))]);
    match ContainerModule.check(&ctx, &absent).await.unwrap() {
        ModuleStatus::Pending { plan } => assert!(plan.contains("Remove container doomed")),
        other => panic!("expected Pending, got {other:?}"),
    }

    let result = ContainerModule.apply(&ctx, &absent).await.unwrap();
    assert!(result.changed);
    assert!(!container_exists(&ssh, "doomed").await);
    assert!(matches!(
        ContainerModule.check(&ctx, &absent).await.unwrap(),
        ModuleStatus::Satisfied
    ));
    // Re-applying an already-absent container is a no-op, not an error.
    let result = ContainerModule.apply(&ctx, &absent).await.unwrap();
    assert!(!result.changed);
}

/// A custom network is created on demand; built-in modes are left alone.
#[tokio::test]
async fn test_custom_network_is_created() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let spec = params(
        "netted",
        &[("image", s("nginx:alpine")), ("network", s("app-net"))],
    );
    ContainerModule.apply(&ctx, &spec).await.unwrap();

    let created = ssh
        .exec(&format!("test -d {}/net/app-net", STATE_ROOT))
        .await
        .unwrap();
    assert_eq!(created.exit_code, 0, "custom network was not created");

    let host_net = params(
        "hostnet",
        &[("image", s("nginx:alpine")), ("network", s("host"))],
    );
    ContainerModule.apply(&ctx, &host_net).await.unwrap();
    let builtin = ssh
        .exec(&format!("test -d {}/net/host", STATE_ROOT))
        .await
        .unwrap();
    assert_ne!(
        builtin.exit_code, 0,
        "built-in network modes must not be created"
    );
}
