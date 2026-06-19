use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn purge_server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_purgery-server")
}

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

/// Run a purgery-server subcommand and return (stdout, stderr).
/// Intentionally simple — no async, no timeout manager, no complex machinery.
fn run_server(config: &std::path::Path, args: &[&str]) -> Result<(String, String), String> {
    let output = Command::new(purge_server_bin())
        .args(["--config", config.to_str().unwrap()])
        .args(args)
        .output()
        .map_err(|e| format!("failed to run server: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!(
            "server command failed: {}\nstderr: {stderr}",
            args.join(" "),
        ));
    }
    Ok((stdout, stderr))
}

#[test]
fn concurrent_process_run_does_not_double_process() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = write_config(&temp);
    let wd = temp.path().join("purgery");

    // Create work/ and storage/ directories
    fs::create_dir_all(&wd).unwrap();
    let final_dir = temp.path().join("final");
    fs::create_dir_all(&final_dir).unwrap();
    let started_marker = temp.path().join("started");
    let release_marker = temp.path().join("release");
    let source_file = temp.path().join("input.txt");
    let source_bytes = b"hello world\n";
    fs::write(&source_file, source_bytes).unwrap();

    // Write server config with a blocking transform
    let transform_config = format!(
        r#"
work_dir = {:?}

[[transform]]
name = "slow-copy"
kind = "subprocess"
program = "/bin/sh"
args = [
    "-c",
    "touch \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.05; done; cp \"$3\" \"$4\"",
    "sh",
    {:?},
    {:?},
    "{{input}}",
    "{{target_directory}}/out.txt",
]
expected_outputs = ["out.txt"]
"#,
        wd.to_string_lossy(),
        started_marker.to_string_lossy(),
        release_marker.to_string_lossy(),
    );
    fs::write(temp.path().join("server.toml"), transform_config).unwrap();

    // Bootstrap server directories
    run_server(&config_path, &["bootstrap"]).unwrap();

    // Create a server run
    let nickname = "laptop";
    let run_id = "concurrency-test-001";

    let (out, _) = run_server(
        &config_path,
        &["begin-run", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();
    let begin: purgery_core::BeginRunResponse = toml::from_str(&out).unwrap();

    // Write run.toml
    fs::write(
        std::path::Path::new(&begin.incoming_dir).join("run.toml"),
        format!(
            r#"purgery_version = "0.1.0"
nickname = "{nickname}"
destination = {:?}
delete_after_import = true
"#,
            final_dir.to_string_lossy(),
        ),
    )
    .unwrap();

    // Write manifest.toml without sha256 (it's optional)
    fs::write(
        std::path::Path::new(&begin.incoming_dir).join("manifest.toml"),
        format!(
            r#"purgery_version = "0.1.0"
run_id = "{run_id}"
nickname = "{nickname}"

[[entries]]
local_path = {:?}
staged_path = "files/input.txt"
relative_path = "input.txt"
kind = "regular_file"
size = {}
mtime_ns = 1700000000000000000
transform = "slow-copy"
"#,
            source_file.to_string_lossy(),
            source_bytes.len(),
        ),
    )
    .unwrap();

    // Create staged file
    let files_dir = std::path::Path::new(&begin.files_dir);
    fs::create_dir_all(files_dir).unwrap();
    fs::write(files_dir.join("input.txt"), source_bytes).unwrap();

    // Prepare and finish the run
    run_server(
        &config_path,
        &["prepare-run", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();
    run_server(
        &config_path,
        &["finish-run", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();

    // Verify run-state shows ready
    let (out, _) = run_server(
        &config_path,
        &["run-state", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();
    let state: purgery_core::RunStateResponse = toml::from_str(&out).unwrap();
    assert_eq!(state.phase, "ready", "run should be in ready phase");

    // === Step 1: Start first process-run (blocking transform) ===
    let mut child1 = Command::new(purge_server_bin())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "process-run",
            "--nickname",
            nickname,
            "--run-id",
            run_id,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn first process-run");

    // Wait for the transform to start (create started marker).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if started_marker.exists() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child1.kill();
            panic!("first process-run did not start transform within 30s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Wait for run-state to show processing + active
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (out, _) = run_server(
            &config_path,
            &["run-state", "--nickname", nickname, "--run-id", run_id],
        )
        .unwrap_or_else(|_| ("".to_string(), "".to_string()));
        if let Ok(s) = toml::from_str::<purgery_core::RunStateResponse>(&out) {
            if s.phase == "processing" && s.processor_state.as_deref() == Some("active") {
                break;
            }
        }
        if Instant::now() > deadline {
            let _ = child1.kill();
            panic!("run did not reach processing+active within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // === Step 2: Run second process-run while first is blocked ===
    let (out, _) = run_server(
        &config_path,
        &["process-run", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();
    let resp2: purgery_core::ProcessRunResponse = toml::from_str(&out).unwrap();
    assert_eq!(
        resp2.outcome, "already_active",
        "second process-run must report already_active, got outcome={}",
        resp2.outcome,
    );
    assert_eq!(
        resp2.run_phase.as_deref(),
        Some("processing"),
        "second process-run must report run_phase=processing",
    );
    assert_eq!(
        resp2.status_state, None,
        "second process-run must not report status_state",
    );

    // === Step 3: Release the first transform ===
    fs::write(&release_marker, "go").unwrap();

    // Wait for first process-run to finish
    let output1 = child1
        .wait_with_output()
        .expect("failed to wait for first process-run");
    assert!(output1.status.success(), "first process-run must succeed");
    let stdout1 = String::from_utf8_lossy(&output1.stdout).to_string();
    let resp1: purgery_core::ProcessRunResponse = toml::from_str(&stdout1).unwrap();
    assert_eq!(
        resp1.outcome, "processed",
        "first process-run must report processed, got outcome={}",
        resp1.outcome,
    );
    assert_eq!(
        resp1.run_phase.as_deref(),
        Some("done"),
        "first process-run must report run_phase=done",
    );
    assert_eq!(
        resp1.status_state.as_deref(),
        Some("done"),
        "first process-run must report status_state=done",
    );

    // === Step 4: Verify final state ===
    // Terminal status must exist and be readable
    let (out, _) = run_server(
        &config_path,
        &["status", "--nickname", nickname, "--run-id", run_id],
    )
    .unwrap();
    let status: purgery_core::RunStatus = purgery_core::RunStatus::from_toml(&out).unwrap();
    assert_eq!(status.state, purgery_core::RunState::Done);

    // Only one output file must exist
    let output_file = final_dir.join("out.txt");
    assert!(output_file.exists(), "output file must exist");
    assert_eq!(
        fs::read(&output_file).unwrap(),
        source_bytes,
        "output must match source input",
    );
}
