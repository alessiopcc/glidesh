mod common;

use glidesh::config::template::interpolate;
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams};
use glidesh::secrets::config::{Provider, SecretsConfig};
use glidesh::secrets::passphrase::{PassphraseProvider, generate_dek};
use glidesh::secrets::{Identity, Secrets, token};
use std::collections::HashMap;

/// A secret value decrypts, flows into an interpolated shell command run over real SSH,
/// and the same plaintext is registered so it would be redacted from any emitted output.
#[tokio::test]
async fn secret_value_reaches_remote_command_and_is_redacted() {
    skip_unless_integration!();

    // Build a passphrase-wrapped DEK and encrypt a value, exactly as the CLI does.
    let dek = generate_dek();
    let wrapped = PassphraseProvider::new("pw".into()).wrap_dek(&dek).unwrap();
    let cfg = SecretsConfig {
        provider: Provider::Passphrase,
        encryptedkey: wrapped,
        recipients: Vec::new(),
    };
    let token = token::encrypt_value(&dek, b"s3cr3t-value").unwrap();

    // Unlock and run the up-front decrypt sweep the executor performs per host.
    let secrets = Secrets::open(Some(&cfg), Some(&Identity::Passphrase("pw".into()))).unwrap();
    let mut vars: HashMap<String, String> = HashMap::from([("db_password".to_string(), token)]);
    secrets.decrypt_vars(&mut vars).unwrap();
    assert_eq!(vars.get("db_password").unwrap(), "s3cr3t-value");

    // The decrypted plaintext interpolates into a command and runs on the container.
    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let command = interpolate("printf '%s' ${db_password}", &vars).unwrap();
    let params = ModuleParams {
        resource_name: command,
        args: HashMap::new(),
    };
    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains("s3cr3t-value"),
        "remote command should receive the plaintext, got: {}",
        result.output
    );

    // The redaction registry masks the secret in any output before it is emitted/logged.
    assert_eq!(
        secrets.registry().redact(&result.output),
        "***",
        "the secret must be redacted from emitted output"
    );
}
