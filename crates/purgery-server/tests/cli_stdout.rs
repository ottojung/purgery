use purgery_core::{BeginRunResponse, Nickname, RunId, RunState, RunStatus};
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
