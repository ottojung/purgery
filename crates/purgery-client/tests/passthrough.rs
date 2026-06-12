use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn passthrough_without_cleanup_runs_rsync_without_server_run() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("commands.log");
    write_executable(
        &bin.join("rsync"),
        "#!/bin/sh\nprintf 'rsync:%s\\n' \"$*\" >> \"$COMMAND_LOG\"\n",
    );
    write_executable(
        &bin.join("ssh"),
        "#!/bin/sh\nprintf 'ssh:%s\\n' \"$*\" >> \"$COMMAND_LOG\"\nexit 99\n",
    );
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), "hello").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--",
            source.to_str().unwrap(),
            "user@host:/absolute/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"));
    assert!(commands.contains("user@host:/absolute/dest/"));
    assert!(
        !commands.contains("ssh:"),
        "passthrough must not create a server run"
    );
}

#[test]
fn passthrough_cleanup_deletes_only_an_unchanged_original() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_executable(&bin.join("rsync"), "#!/bin/sh\nexit 0\n");
    write_executable(&bin.join("ssh"), "#!/bin/sh\nexit 99\n");
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    let file = source.join("a.txt");
    fs::write(&file, "hello").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--delete-after-import",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            source.to_str().unwrap(),
            "host:relative/dest",
        ])
        .env("PATH", &bin)
        .status()
        .unwrap();

    assert!(status.success());
    assert!(!file.exists());
}

#[test]
fn passthrough_cleanup_preserves_an_original_changed_during_rsync() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_executable(
        &bin.join("rsync"),
        "#!/bin/sh\nprintf changed > \"$CHANGED_FILE\"\n",
    );
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    let file = source.join("a.txt");
    fs::write(&file, "hello").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--delete-after-import",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            source.to_str().unwrap(),
            "host:relative/dest",
        ])
        .env("PATH", &bin)
        .env("CHANGED_FILE", &file)
        .status()
        .unwrap();

    assert!(status.success());
    assert_eq!(fs::read_to_string(file).unwrap(), "changed");
}

#[test]
fn postprocess_requires_delete_after_import_before_any_command_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), "hello").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--postprocess",
            "transform",
            "--",
            source.to_str().unwrap(),
            "host:/destination",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--delete-after-import is required when --postprocess is used"));
}
