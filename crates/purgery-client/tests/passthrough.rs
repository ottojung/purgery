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
fn passthrough_relative_destination_uses_rsync_directly_no_server_run() {
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
            "user@host:relative/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"), "passthrough must run rsync");
    assert!(
        commands.contains("relative/dest/"),
        "rsync must receive relative destination"
    );
    assert!(
        !commands.contains("ssh:"),
        "passthrough with relative destination must not create a server run"
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

#[test]
fn passthrough_split_dot_runs_ordinary_rsync_no_filter() {
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
    fs::write(source.join("a.mp4"), "data").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            ".",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            source.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"));
    assert!(!commands.contains("ssh:"));
    // --split "." uses ordinary rsync with --recursive (no filter rules)
    assert!(
        commands.contains("--recursive"),
        "--split '.' must use ordinary rsync with --recursive"
    );
    // No include/exclude filter rules
    assert!(
        !commands.contains("--include"),
        "--split '.' must not use --include filters"
    );
    assert!(
        !commands.contains("--exclude"),
        "--split '.' must not use --exclude filters"
    );
    // Source should not have trailing slash (source-entry semantics).
    // In rsync argv, the -- separator precedes the source operand.
    let source_str = source.to_str().unwrap();
    assert!(
        !commands.contains(&format!("-- {source_str}/")),
        "source operand must not have trailing slash for --split '.'"
    );
}

#[test]
fn passthrough_split_filter_mode_uses_include_exclude_no_quotes() {
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
    fs::write(source.join("a.mp4"), "data").unwrap();
    fs::write(source.join("b.txt"), "text").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            "*.mp4",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            source.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"));
    assert!(!commands.contains("ssh:"));
    // Filter rules must not have shell quotes in the actual argv
    assert!(commands.contains("--include=*/"));
    assert!(commands.contains("--exclude=*"));
    assert!(!commands.contains("--include='"));
    assert!(!commands.contains("--exclude='"));
    // Source operand has trailing slash in filter mode.
    // In rsync argv, the -- separator precedes the source operand.
    let source_str = source.to_str().unwrap();
    assert!(
        commands.contains(&format!("-- {source_str}/")),
        "source operand must have trailing slash in filter mode"
    );
}

#[test]
fn passthrough_trailing_slash_does_not_change_source_entry_semantics() {
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
    let source = tmp.path().join("Videos");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.mp4"), "data").unwrap();

    let source_with_slash = format!("{}/", source.to_str().unwrap());

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args(["sync", "--", &source_with_slash, "user@host:/dest"])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    // The source operand should not have the trailing slash (source-entry semantics).
    // In rsync argv, -- precedes the source, then the destination.
    let source_str = source.to_str().unwrap();
    let trailing_slash_operand = format!("-- {source_str}/ ");
    assert!(
        !commands.contains(&trailing_slash_operand),
        "trailing slash must not reach rsync source operand: found '{}'",
        trailing_slash_operand
    );
    // Should import as source entry, not contents
    assert!(commands.contains("--recursive"), "must use ordinary rsync");
}

#[test]
fn passthrough_split_dot_on_regular_file_runs_ordinary_rsync() {
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
    let file = source.join("a.mp4");
    fs::write(&file, "data").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            ".",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            file.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"));
    assert!(!commands.contains("ssh:"));
    assert!(!commands.contains("--include"));
    assert!(!commands.contains("--exclude"));
    assert!(
        !commands.contains("--prune-empty-dirs"),
        "ordinary rsync must not use --prune-empty-dirs"
    );
}

#[test]
fn passthrough_split_non_dot_on_regular_file_does_not_call_rsync() {
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
    let file = source.join("a.mp4");
    fs::write(&file, "data").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            "*.mp4",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            file.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(
        !commands.contains("rsync:"),
        "non-directory source with non-dot split must not invoke rsync"
    );
    assert!(!commands.contains("ssh:"));
}

#[test]
fn passthrough_split_dot_on_symlink_runs_ordinary_rsync() {
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
    let target_file = source.join("real.mp4");
    fs::write(&target_file, "data").unwrap();
    let link = source.join("link.mp4");
    std::os::unix::fs::symlink(&target_file, &link).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            ".",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            link.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("rsync:"));
    assert!(!commands.contains("ssh:"));
}

#[test]
fn passthrough_split_non_dot_on_symlink_does_not_call_rsync() {
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
    let target_file = source.join("real.mp4");
    fs::write(&target_file, "data").unwrap();
    let link = source.join("link.mp4");
    std::os::unix::fs::symlink(&target_file, &link).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_purgery-client"))
        .args([
            "sync",
            "--split",
            "*.mp4",
            "--state-dir",
            tmp.path().join("state").to_str().unwrap(),
            "--",
            link.to_str().unwrap(),
            "user@host:/dest",
        ])
        .env("PATH", &bin)
        .env("COMMAND_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(
        !commands.contains("rsync:"),
        "symlink source with non-dot split must not invoke rsync"
    );
    assert!(!commands.contains("ssh:"));
}
