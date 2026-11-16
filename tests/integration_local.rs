//! `run --local` inside the container, as a Dockerfile `RUN` would use it: the glidesh binary is
//! copied in and applies a plan to the container itself, with no SSH between them. Run by the
//! unprivileged `deploy` user escalating with sudo, so a local host's run-as, staged uploads
//! included, is covered too.
//!
//! The test SSH session only puts the binary and the plan in place and reads the results.

mod common;

const PLAN: &str = r#"plan "image" {
    step "User" {
        user "appsvc" {
            shell "/usr/sbin/nologin"
        }
    }
    step "Config" {
        file "/etc/glidesh-local.conf" src="app.conf" template=#true owner="appsvc" mode="0600"
    }
    step "Package" {
        package "curl"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn run_local_applies_a_plan_inside_the_container_and_then_changes_nothing() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let binary = std::fs::read(env!("CARGO_BIN_EXE_glidesh")).unwrap();
    ssh.upload_file(&binary, "/usr/local/bin/glidesh")
        .await
        .unwrap();
    let setup = ssh
        .exec(
            "chmod 755 /usr/local/bin/glidesh && mkdir -p /srv/plan && \
             printf 'host=${@host.name} os=${@os.family}\\n' > /srv/plan/app.conf",
        )
        .await
        .unwrap();
    assert_eq!(setup.exit_code, 0, "{}", setup.stderr);
    ssh.upload_file(PLAN.as_bytes(), "/srv/plan/plan.kdl")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let run = "cd /srv/plan && glidesh run --local -p plan.kdl --no-tui --run-as root 2>&1";
    let first = deploy.exec(run).await.unwrap();
    assert_eq!(first.exit_code, 0, "{}", first.stdout);
    assert!(first.stdout.contains(" 2 changed"), "{}", first.stdout);

    let conf = ssh
        .exec("stat -c '%U %a' /etc/glidesh-local.conf && cat /etc/glidesh-local.conf")
        .await
        .unwrap();
    assert_eq!(conf.stdout, "appsvc 600\nhost=local os=debian\n");

    let second = deploy.exec(run).await.unwrap();
    assert_eq!(second.exit_code, 0, "{}", second.stdout);
    assert!(second.stdout.contains(" 0 changed"), "{}", second.stdout);
}
