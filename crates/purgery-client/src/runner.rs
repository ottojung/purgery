use anyhow::{Context, Result};
use purgery_core::{shell_escape, BeginRunResponse, PrepareRunResponse, RunStateResponse};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Pre-scripted response for a remote server command.
/// The `cmd_contains` string must be a substring of the full SSH command.
#[derive(Debug, Clone)]
struct ScriptedResponse {
    cmd_contains: String,
    stdout: String,
}

/// A command-execution backend that either runs real SSH/rsync or returns
/// scripted responses for testing.
///
/// Clone is cheap: for Real it's a trivial copy; for Fake it shares the
/// underlying state via Arc so all clones see the same scripted responses
/// and log.
#[derive(Clone)]
pub(crate) enum RemoteRunner {
    Real,
    #[allow(dead_code)]
    Fake {
        inner: Arc<FakeState>,
    },
}

pub(crate) struct FakeState {
    responses: Mutex<Vec<ScriptedResponse>>,
    errors: Mutex<Vec<(String, String)>>,
    write_errors: Mutex<Vec<(String, String)>>,
    rsync_errors: Mutex<Vec<(String, String)>>,
    log: Mutex<Vec<String>>,
    written_files: Mutex<HashMap<String, String>>,
    finish_run_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
    rsync_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl std::fmt::Debug for FakeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeState")
            .field("responses", &self.responses)
            .field("errors", &self.errors)
            .field("write_errors", &self.write_errors)
            .field("rsync_errors", &self.rsync_errors)
            .field("log", &self.log)
            .field("written_files", &self.written_files)
            .finish()
    }
}

impl RemoteRunner {
    pub(crate) fn real() -> Self {
        RemoteRunner::Real
    }

    #[allow(dead_code)]
    pub(crate) fn fake() -> Self {
        RemoteRunner::Fake {
            inner: Arc::new(FakeState {
                responses: Mutex::new(Vec::new()),
                errors: Mutex::new(Vec::new()),
                write_errors: Mutex::new(Vec::new()),
                rsync_errors: Mutex::new(Vec::new()),
                log: Mutex::new(Vec::new()),
                written_files: Mutex::new(HashMap::new()),
                finish_run_hook: Mutex::new(None),
                rsync_hook: Mutex::new(None),
            }),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn add_response(&self, cmd_contains: &str, stdout: &str) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner.responses.lock().unwrap().push(ScriptedResponse {
                    cmd_contains: cmd_contains.to_owned(),
                    stdout: stdout.to_owned(),
                });
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn add_error(&self, cmd_contains: &str, error_msg: &str) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner
                    .errors
                    .lock()
                    .unwrap()
                    .push((cmd_contains.to_owned(), error_msg.to_owned()));
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn add_write_error(&self, path_contains: &str, error_msg: &str) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner
                    .write_errors
                    .lock()
                    .unwrap()
                    .push((path_contains.to_owned(), error_msg.to_owned()));
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub(crate) fn set_finish_run_hook(&self, hook: Box<dyn Fn() + Send>) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner.finish_run_hook.lock().unwrap().replace(hook);
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_rsync_hook(&self, hook: Box<dyn Fn() + Send>) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner.rsync_hook.lock().unwrap().replace(hook);
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn add_rsync_error(&self, cmd_contains: &str, error_msg: &str) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner
                    .rsync_errors
                    .lock()
                    .unwrap()
                    .push((cmd_contains.to_owned(), error_msg.to_owned()));
            }
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn command_log(&self) -> Vec<String> {
        match self {
            RemoteRunner::Fake { inner } => inner.log.lock().unwrap().clone(),
            _ => unreachable!(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn written_files(&self) -> HashMap<String, String> {
        match self {
            RemoteRunner::Fake { inner } => inner.written_files.lock().unwrap().clone(),
            _ => unreachable!(),
        }
    }

    /// Run a purgery-server command on the remote host via SSH.
    /// Returns stdout on success.
    pub(crate) fn server_cmd(&self, host: &str, server_cmd: &str, args: &[&str]) -> Result<String> {
        let full_cmd = {
            let mut cmd = server_cmd.to_owned();
            for a in args {
                cmd.push(' ');
                cmd.push_str(&shell_escape(a));
            }
            cmd
        };
        let ssh_cmd = format!("ssh -- {host} {full_cmd}");

        match self {
            RemoteRunner::Real => {
                let output = Command::new("ssh")
                    .arg("--")
                    .arg(host)
                    .arg(&full_cmd)
                    .output()
                    .with_context(|| format!("failed to execute SSH command on {host}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("SSH command on {host} failed: {stderr}");
                }
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            }
            RemoteRunner::Fake { inner } => {
                inner.log.lock().unwrap().push(ssh_cmd.clone());
                for (ec, err) in inner.errors.lock().unwrap().iter() {
                    if ssh_cmd.contains(ec.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                if ssh_cmd.contains("finish-run") {
                    if let Some(hook) = inner.finish_run_hook.lock().unwrap().take() {
                        hook();
                    }
                }
                for resp in inner.responses.lock().unwrap().iter().rev() {
                    if ssh_cmd.contains(&resp.cmd_contains) {
                        return Ok(resp.stdout.clone());
                    }
                }
                anyhow::bail!(
                    "no scripted response for command (did you forget add_response?), \
                     cmd was: {ssh_cmd}"
                )
            }
        }
    }

    /// Write a file on the remote host via SSH.
    pub(crate) fn write_remote_file(&self, host: &str, path: &str, content: &str) -> Result<()> {
        match self {
            RemoteRunner::Real => {
                let remote_cmd = format!("cat > {}", shell_escape(path));
                let mut child = Command::new("ssh")
                    .arg("--")
                    .arg(host)
                    .arg(&remote_cmd)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .with_context(|| format!("failed to spawn SSH write to {host}:{path}"))?;
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin
                        .write_all(content.as_bytes())
                        .with_context(|| "failed to write content to SSH stdin")?;
                }
                let output = child
                    .wait_with_output()
                    .with_context(|| "failed to wait for SSH write")?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("failed to write remote file {path} on {host}: {stderr}");
                }
                Ok(())
            }
            RemoteRunner::Fake { inner } => {
                inner
                    .log
                    .lock()
                    .unwrap()
                    .push(format!("write {host}:{path}"));
                for (pc, err) in inner.write_errors.lock().unwrap().iter() {
                    if path.contains(pc.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                inner
                    .written_files
                    .lock()
                    .unwrap()
                    .insert(path.to_owned(), content.to_owned());
                Ok(())
            }
        }
    }

    /// Run rsync to transfer files to a remote host.
    pub(crate) fn run_rsync(&self, source: &str, host: &str, remote_dir: &str) -> Result<()> {
        let rsync_dest = format!("{host}:{remote_dir}/");
        let rsync_cmd = format!(
            "rsync --recursive --partial --inplace --mkpath --archive --protect-args -- {source} {rsync_dest}"
        );

        match self {
            RemoteRunner::Real => {
                let rsync_args = purgery_core::build_rsync_args(source, &rsync_dest);
                let status = Command::new("rsync")
                    .args(&rsync_args)
                    .status()
                    .with_context(|| {
                        format!("failed to execute rsync: {source} -> {host}:{remote_dir}")
                    })?;
                if !status.success() {
                    anyhow::bail!("rsync failed: {source} -> {host}:{remote_dir}");
                }
                Ok(())
            }
            RemoteRunner::Fake { inner } => {
                // Call rsync hook before processing errors and log, so
                // tests using a blocking hook can synchronize with staging.
                if let Some(hook) = inner.rsync_hook.lock().unwrap().take() {
                    hook();
                }
                for (rc, err) in inner.rsync_errors.lock().unwrap().iter() {
                    if rsync_cmd.contains(rc.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                inner.log.lock().unwrap().push(rsync_cmd);
                Ok(())
            }
        }
    }

    /// Run rsync with --files-from for selected-file-only transfer.
    pub(crate) fn run_rsync_with_file_list(
        &self,
        source: &str,
        host: &str,
        remote_dir: &str,
        file_list: &[String],
    ) -> Result<()> {
        let rsync_dest = format!("{host}:{remote_dir}/");

        // All options before --.  Source/dest operands after --.
        let args: Vec<String> = vec![
            "--recursive".to_string(),
            "--partial".to_string(),
            "--inplace".to_string(),
            "--mkpath".to_string(),
            "--archive".to_string(),
            "--protect-args".to_string(),
            "--files-from={FILES}".to_string(),
            "--relative".to_string(),
            "--from0".to_string(),
            "--".to_string(),
            format!("{}/", source),
            rsync_dest.clone(),
        ];

        match self {
            RemoteRunner::Real => {
                let tmp_dir = std::env::temp_dir();
                let list_path = tmp_dir.join(format!("purgery-rsync-{}.list", std::process::id()));
                let list_content = file_list.join("\0");
                let files_arg = format!("--files-from={}", list_path.display());

                std::fs::write(&list_path, &list_content)
                    .with_context(|| "failed to write rsync file list")?;
                let result = (|| -> Result<()> {
                    let real_args: Vec<String> = args
                        .iter()
                        .map(|a| {
                            if a == "--files-from={FILES}" {
                                files_arg.clone()
                            } else {
                                a.clone()
                            }
                        })
                        .collect();
                    let status = Command::new("rsync")
                        .args(&real_args)
                        .status()
                        .with_context(|| {
                            format!("failed to execute rsync: {source} -> {host}:{remote_dir}")
                        })?;
                    if !status.success() {
                        anyhow::bail!("rsync failed: {source} -> {host}:{remote_dir}");
                    }
                    Ok(())
                })();
                let _ = std::fs::remove_file(&list_path);
                result
            }
            RemoteRunner::Fake { inner } => {
                if let Some(hook) = inner.rsync_hook.lock().unwrap().take() {
                    hook();
                }
                for (rc, err) in inner.rsync_errors.lock().unwrap().iter() {
                    let cmd = args.join(" ");
                    if cmd.contains(rc.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                inner.log.lock().unwrap().push(args.join(" "));
                Ok(())
            }
        }
    }

    /// Parse a BeginRunResponse from the given TOML string. Convenience for tests.
    pub(crate) fn parse_begin_response(toml: &str) -> Result<BeginRunResponse> {
        let resp: BeginRunResponse =
            toml::from_str(toml).with_context(|| "failed to parse begin-run response")?;
        if resp.protocol_version != 1 {
            anyhow::bail!("unsupported protocol version");
        }
        Ok(resp)
    }

    /// Parse a PrepareRunResponse from the given TOML string.
    pub(crate) fn parse_prepare_response(toml: &str) -> Result<PrepareRunResponse> {
        let resp: PrepareRunResponse =
            toml::from_str(toml).with_context(|| "failed to parse prepare-run response")?;
        if resp.protocol_version != 1 {
            anyhow::bail!("unsupported protocol version");
        }
        Ok(resp)
    }

    /// Parse a RunStateResponse from the given TOML string.
    pub(crate) fn parse_run_state_response(toml: &str) -> Result<RunStateResponse> {
        toml::from_str(toml).with_context(|| "failed to parse run-state response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_runner_returns_scripted_responses() {
        let runner = RemoteRunner::fake();
        runner.add_response("begin-run", "protocol_version = 1\n");
        runner.add_response("prepare-run", "protocol_version = 1\n");

        let out = runner.server_cmd("host", "ps", &["begin-run"]).unwrap();
        assert_eq!(out, "protocol_version = 1\n");

        let out = runner.server_cmd("host", "ps", &["prepare-run"]).unwrap();
        assert_eq!(out, "protocol_version = 1\n");

        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("begin-run")));
        assert!(log.iter().any(|c| c.contains("prepare-run")));
    }

    #[test]
    fn fake_runner_errors_on_unrecognized_command() {
        let runner = RemoteRunner::fake();
        let result = runner.server_cmd("host", "ps", &["unknown-command"]);
        assert!(result.is_err());
    }

    #[test]
    fn fake_runner_add_error_overrides_response() {
        let runner = RemoteRunner::fake();
        runner.add_response("begin-run", "ok");
        runner.add_error("begin-run", "simulated failure");
        let result = runner.server_cmd("host", "ps", &["begin-run"]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated failure"));
    }

    #[test]
    fn fake_runner_write_error_stops_write() {
        let runner = RemoteRunner::fake();
        runner.add_write_error("manifest.toml", "write failed");
        let result = runner.write_remote_file("host", "/remote/manifest.toml", "content");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("write failed"));
    }

    #[test]
    fn fake_runner_write_error_does_not_write_file() {
        let runner = RemoteRunner::fake();
        runner.add_write_error("manifest.toml", "write failed");
        let _ = runner.write_remote_file("host", "/remote/manifest.toml", "content");
        let files = runner.written_files();
        assert!(!files.contains_key("/remote/manifest.toml"));
    }

    #[test]
    fn fake_runner_write_succeeds_when_no_error_matches() {
        let runner = RemoteRunner::fake();
        runner.add_write_error("other.toml", "write failed");
        let result = runner.write_remote_file("host", "/remote/manifest.toml", "content");
        assert!(result.is_ok());
        let files = runner.written_files();
        assert_eq!(files.get("/remote/manifest.toml").unwrap(), "content");
    }

    #[test]
    fn fake_runner_rsync_error_stops_transfer() {
        let runner = RemoteRunner::fake();
        runner.add_rsync_error("host", "rsync failed");
        let result = runner.run_rsync("/src", "host", "/dest");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rsync failed"));
    }

    #[test]
    fn fake_runner_rsync_succeeds_when_no_error_matches() {
        let runner = RemoteRunner::fake();
        runner.add_rsync_error("other-host", "rsync failed");
        let result = runner.run_rsync("/src", "host", "/dest");
        assert!(result.is_ok());
    }

    #[test]
    fn file_list_rsync_argv_puts_options_before_separator() {
        let runner = RemoteRunner::fake();
        let files = vec!["a.mp4".to_string(), "b.mp4".to_string()];
        runner
            .run_rsync_with_file_list("/src", "host", "/dest", &files)
            .unwrap();
        let log = runner.command_log();
        assert_eq!(log.len(), 1, "must be exactly one rsync process");
        let cmd = &log[0];
        // Verify all options appear before -- and operands after.
        let before_sep = cmd.split(" -- ").next().unwrap();
        assert!(
            before_sep.contains("--files-from"),
            "--files-from must be before --"
        );
        assert!(
            before_sep.contains("--relative"),
            "--relative must be before --"
        );
        assert!(before_sep.contains("--from0"), "--from0 must be before --");
        assert!(
            cmd.contains("/src/"),
            "source dir with trailing / must be after --"
        );
        assert!(
            cmd.contains("host:/dest/"),
            "dest with trailing / must be after --"
        );
    }

    #[test]
    fn pure_passthrough_split_uses_one_rsync_no_ssh() {
        let runner = RemoteRunner::fake();
        let files = vec!["a.mp4".to_string()];
        runner
            .run_rsync_with_file_list("/src", "host", "/dest", &files)
            .unwrap();
        let log = runner.command_log();
        assert_eq!(log.len(), 1, "pure passthrough split must use one rsync");
        assert!(
            !log[0].contains("ssh"),
            "must be an rsync command, not ssh"
        );
    }
}
