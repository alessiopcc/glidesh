use glidesh::modules::context::ModuleContext;
use glidesh::modules::detect::{OsInfo, detect_os};
use glidesh::ssh::SshSession;
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

/// Check if integration tests should run.
/// Returns `true` if `GLIDESH_INTEGRATION` env var is set.
pub fn should_run() -> bool {
    std::env::var("GLIDESH_INTEGRATION").is_ok()
}

/// Macro to skip a test if GLIDESH_INTEGRATION is not set.
#[macro_export]
macro_rules! skip_unless_integration {
    () => {
        if !common::should_run() {
            eprintln!("Skipping integration test (set GLIDESH_INTEGRATION=1 to enable)");
            return;
        }
    };
}

/// Generate an ed25519 keypair. Returns (private key for auth, OpenSSH public key string).
pub fn generate_keypair() -> (PrivateKeyWithHashAlg, String) {
    let private = ssh_key::PrivateKey::random(&mut rand::thread_rng(), ssh_key::Algorithm::Ed25519)
        .expect("failed to generate ed25519 key");

    let pubkey_str = private
        .public_key()
        .to_openssh()
        .expect("failed to serialize public key");

    let key = PrivateKeyWithHashAlg::new(Arc::new(private), None)
        .expect("failed to create PrivateKeyWithHashAlg");

    (key, pubkey_str)
}

/// A Docker container running Ubuntu with SSH + systemd for integration testing.
pub struct TestContainer {
    pub container_id: String,
    pub port: u16,
    pub key: PrivateKeyWithHashAlg,
    image_tag: String,
}

impl TestContainer {
    /// Build the Docker image and start a privileged container.
    /// Returns a TestContainer with the mapped SSH port.
    pub fn start() -> Self {
        let (key, pubkey) = generate_keypair();
        let suffix: u32 = rand::random::<u32>() % 100_000;
        let container_name = format!("glidesh-test-{}", suffix);
        let image_tag = format!("glidesh-test-img:{}", suffix);
        let dockerfile_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");

        // Build the Docker image with a unique tag to avoid races between parallel tests
        let build = Command::new("docker")
            .args([
                "build",
                "--build-arg",
                &format!("PUBKEY={}", pubkey),
                "-t",
                &image_tag,
                dockerfile_dir,
            ])
            .output()
            .expect("failed to run docker build");
        assert!(
            build.status.success(),
            "docker build failed: {}",
            String::from_utf8_lossy(&build.stderr)
        );

        // Run the container
        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--privileged",
                "--name",
                &container_name,
                "-p",
                "0:22",
                &image_tag,
            ])
            .output()
            .expect("failed to run docker run");
        assert!(
            run.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let container_id = String::from_utf8_lossy(&run.stdout).trim().to_string();

        // Get the mapped port
        let port_output = Command::new("docker")
            .args(["port", &container_id, "22"])
            .output()
            .expect("failed to get docker port");
        let port_str = String::from_utf8_lossy(&port_output.stdout);
        // Format: "0.0.0.0:12345\n" or ":::12345\n"
        let port: u16 = port_str
            .lines()
            .find_map(|line| line.rsplit(':').next()?.parse().ok())
            .expect("failed to parse mapped port");

        // Wait for SSH port to accept TCP connections
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                std::time::Duration::from_secs(1),
            )
            .is_ok()
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("SSH port {} not reachable after 30s", port);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // Brief settle time for sshd to finish initialization
        std::thread::sleep(std::time::Duration::from_secs(1));

        TestContainer {
            container_id,
            port,
            key,
            image_tag,
        }
    }

    /// Connect an SSH session to this container, with retries.
    pub async fn ssh_session(&self) -> SshSession {
        let mut last_err = None;
        for attempt in 0..15 {
            match SshSession::connect(
                "127.0.0.1",
                self.port,
                "root",
                &self.key,
                glidesh::ssh::HostKeyPolicy {
                    verify: false,
                    accept_new: false,
                },
            )
            .await
            {
                Ok(session) => return session,
                Err(e) => {
                    last_err = Some(e);
                    let delay = std::cmp::min(1000 * (attempt + 1), 5000);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
        panic!(
            "Failed to connect to test container after 15 attempts: {:?}",
            last_err
        );
    }

    /// Detect the OS on the container.
    pub async fn detect_os(&self, ssh: &SshSession) -> OsInfo {
        detect_os(ssh).await.expect("failed to detect OS")
    }

    /// Create a ModuleContext for testing.
    #[allow(dead_code)]
    pub fn module_context<'a>(
        &self,
        ssh: &'a SshSession,
        os_info: &'a OsInfo,
        vars: &'a HashMap<String, String>,
        dry_run: bool,
    ) -> ModuleContext<'a> {
        ModuleContext {
            ssh,
            os_info,
            vars,
            template_data: &glidesh::config::template::EMPTY_TEMPLATE_DATA,
            dry_run,
            plan_base_dir: std::path::Path::new("."),
        }
    }
}

impl Drop for TestContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .output();
        let _ = Command::new("docker")
            .args(["rmi", "-f", &self.image_tag])
            .output();
    }
}

/// Set up a loopback device on the container. Returns the device path (e.g. "/dev/loop0").
#[allow(dead_code)]
pub async fn setup_loopback(ssh: &SshSession) -> String {
    let output = ssh
        .exec("losetup --find --show /opt/fake.img")
        .await
        .expect("failed to setup loopback");
    assert_eq!(output.exit_code, 0, "losetup failed: {}", output.stderr);
    output.stdout.trim().to_string()
}

/// Tear down a loopback device.
#[allow(dead_code)]
pub async fn teardown_loopback(ssh: &SshSession, device: &str) {
    let _ = ssh.exec(&format!("losetup -d {}", device)).await;
}
