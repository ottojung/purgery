use purgery_core::{BeginRunResponse, Nickname, ProtocolErrorResponse, RunId, RunState, RunStatus};
use std::fs;
use std::process::Command;

fn write_config(temp: &tempfile::TempDir) -> std::path::PathBuf {
    let config_path = temp.path().join("server.toml");
    let work_dir = temp.path().join("purgery");
    fs::write(
        &config_path,
        format!("work_dir = {:?}\n", work_dir.to_string_lossy()),
    )
    .unwrap();
    config_path
}

#[test]
fn debug_logging_does_not_contaminate_begin_run_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(&temp);
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--log-level",
            "debug",
            "begin-run",
            "--nickname",
            "laptop",
            "--run-id",
            "stdout-begin",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: BeginRunResponse = toml::from_slice(&output.stdout).unwrap();
    assert_eq!(response.nickname, "laptop");
    assert_eq!(response.run_id, "stdout-begin");
}

#[test]
fn debug_logging_does_not_contaminate_status_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(&temp);
    let nickname = Nickname::new("laptop".into()).unwrap();
    let run_id = RunId::new("stdout-status".into()).unwrap();
    let failed = temp.path().join("purgery/laptop/failed/stdout-status");
    fs::create_dir_all(&failed).unwrap();
    let status = RunStatus {
        purgery_version: "0.1.0-test".to_string(),
        run_id,
        nickname,
        state: RunState::Failed,
        entries: vec![],
        error: Some("test failure".into()),
    };
    fs::write(failed.join("status.toml"), status.to_toml().unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--log-level",
            "debug",
            "status",
            "--nickname",
            "laptop",
            "--run-id",
            "stdout-status",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = RunStatus::from_toml(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(parsed.state, RunState::Failed);
}

#[test]
fn verbose_conflicts_with_explicit_log_level() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(&temp);
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--verbose",
            "--log-level",
            "trace",
            "check",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

#[test]
fn protocol_command_without_config_emits_protocol_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args(["begin-run", "--nickname", "laptop", "--run-id", "no-config"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let envelope: ProtocolErrorResponse =
        toml::from_slice(&output.stdout).expect("should be valid protocol error TOML");
    assert!(!envelope.ok);
    assert_eq!(envelope.command, "begin-run");
    assert_eq!(envelope.error.code, "server_config_invalid");
    assert!(
        !envelope.error.message.is_empty(),
        "error message should be non-empty",
    );
}

#[test]
fn operator_command_without_config_emits_cli_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args(["check"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    // Operator commands should emit plain stderr, not a protocol TOML envelope on stdout.
    assert!(
        !output.stderr.is_empty(),
        "operator command should report failure on stderr",
    );
    let have_protocol_envelope: Result<ProtocolErrorResponse, _> = toml::from_slice(&output.stdout);
    assert!(
        have_protocol_envelope.is_err(),
        "operator command should not emit protocol TOML on stdout",
    );
}

#[test]
fn protocol_command_with_invalid_nickname_emits_invalid_request() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(&temp);
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "begin-run",
            "--nickname",
            "invalid nickname!!!",
            "--run-id",
            "test-request",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let envelope: ProtocolErrorResponse =
        toml::from_slice(&output.stdout).expect("should be valid protocol error TOML");
    assert_eq!(envelope.error.code, "invalid_request");
    assert!(
        envelope.error.message.contains("invalid nickname"),
        "error message should mention invalid nickname, got: {}",
        envelope.error.message,
    );
}

#[test]
fn prepare_run_config_invalid_emits_run_plan_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(&temp);
    // No run directory prepared, so prepare-run should fail with run_plan_invalid.
    let output = Command::new(env!("CARGO_BIN_EXE_purgery-server"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "prepare-run",
            "--nickname",
            "laptop",
            "--run-id",
            "no-directory",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let envelope: ProtocolErrorResponse =
        toml::from_slice(&output.stdout).expect("should be valid protocol error TOML");
    assert_eq!(envelope.error.code, "run_plan_invalid");
    assert!(
        !envelope.error.message.is_empty(),
        "error message should be non-empty",
    );
}
