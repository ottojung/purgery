use anyhow::{Context, Result};
use purgery_core::{shell_escape, BeginRunResponse, PrepareRunResponse, RunStateResponse};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Result of a spawned remote command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteCommandExit {
    /// Command exited with status 0.
    Success,
    /// Command exited with nonzero status (remote semantic failure).
    RemoteFailure {
        exit_code: Option<i32>,
        stderr: String,
    },
    /// SSH transport failure (exit code 255, local spawn error, etc.).
    TransportFailure {
        exit_code: Option<i32>,
        details: String,
    },
    /// Locally killed.
    Killed,
}

/// A handle to a remotely spawned server command that may be long-running.
pub(crate) struct RemoteCommandHandle {
    kind: RemoteCommandHandleKind,
    #[allow(dead_code)]
    spawned_cmd_contains: String,
}

enum RemoteCommandHandleKind {
    Real {
        child: Option<std::process::Child>,
    },
    Fake {
        /// How many polls before the fake command "finishes".
        /// None = never finishes (busy), Some(n) = finishes after n polls with given exit.
        remaining_polls: Arc<Mutex<Option<(u64, RemoteCommandExit)>>>,
        exit: Arc<Mutex<Option<RemoteCommandExit>>>,
    },
}

impl RemoteCommandHandle {
    /// Try to check if the command has exited. Returns `Ok(None)` if still running,
    /// `Ok(Some(exit))` if finished, or `Err` on waitpid failure.
    pub(crate) fn try_wait(&mut self) -> Result<Option<RemoteCommandExit>> {
        match &mut self.kind {
            RemoteCommandHandleKind::Real { child } => {
                if let Some(child) = child.as_mut() {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let exit_code = status.code();
                            if status.success() {
                                Ok(Some(RemoteCommandExit::Success))
                            } else if exit_code == Some(255) {
                                Ok(Some(RemoteCommandExit::TransportFailure {
                                    exit_code,
                                    details: "SSH exit code 255".to_string(),
                                }))
                            } else {
                                // Read stderr with bounded capture (64 KiB max).
                                const MAX_STDERR_BYTES: usize = 65536;
                                let stderr = child
                                    .stderr
                                    .take()
                                    .map(|s| {
                                        let mut buf = String::new();
                                        use std::io::Read;
                                        let mut limited = s.take(MAX_STDERR_BYTES as u64 + 1);
                                        let _ = limited.read_to_string(&mut buf);
                                        if buf.len() > MAX_STDERR_BYTES {
                                            buf.truncate(MAX_STDERR_BYTES);
                                            buf.push_str("...(truncated)");
                                        }
                                        buf
                                    })
                                    .unwrap_or_default();
                                Ok(Some(RemoteCommandExit::RemoteFailure {
                                    exit_code,
                                    stderr: stderr.trim().to_string(),
                                }))
                            }
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Ok(Some(RemoteCommandExit::TransportFailure {
                            exit_code: None,
                            details: format!("waitpid error: {e}"),
                        })),
                    }
                } else {
                    Ok(Some(RemoteCommandExit::Killed))
                }
            }
            RemoteCommandHandleKind::Fake {
                remaining_polls,
                exit,
            } => {
                // Decrement the poll counter.
                let mut rem = remaining_polls.lock().unwrap();
                if let Some((count, ref result)) = rem.as_mut() {
                    if *count == 0 {
                        let res = result.clone();
                        exit.lock().unwrap().get_or_insert(res);
                        *rem = None;
                    } else {
                        *count -= 1;
                    }
                }
                Ok(exit.lock().unwrap().clone())
            }
        }
    }

    /// Kill the command.
    pub(crate) fn kill(&mut self) -> Result<()> {
        match &mut self.kind {
            RemoteCommandHandleKind::Real { child } => {
                if let Some(child) = child.as_mut() {
                    child.kill()?;
                }
                Ok(())
            }
            RemoteCommandHandleKind::Fake { .. } => Ok(()),
        }
    }

    /// Block until the remote command exits.  For the real runner this
    /// polls `try_wait` with a small delay.  For the fake runner it
    /// returns the scripted exit.
    ///
    /// Fake-only escape hatch: if no exit was ever scripted (the
    /// command was created without a matching
    /// `add_spawned_cmd_exit`), we return `Killed` immediately so
    /// callers do not hang on fire-and-forget handles that tests never
    /// configured.
    #[allow(dead_code)]
    pub(crate) fn wait(&mut self) -> Result<RemoteCommandExit> {
        if let RemoteCommandHandleKind::Fake {
            remaining_polls,
            exit,
        } = &self.kind
        {
            if remaining_polls.lock().unwrap().is_none() && exit.lock().unwrap().is_none() {
                return Ok(RemoteCommandExit::Killed);
            }
        }
        loop {
            match self.try_wait()? {
                Some(exit) => return Ok(exit),
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }
}

/// Information about a simulated spawned command for tests.
#[allow(dead_code)]
pub(crate) struct SpawnedCommandInfo {
    pub cmd_contains: String,
}

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
    spawned_cmd_exits: Mutex<Vec<(String, usize, RemoteCommandExit)>>,
    write_errors: Mutex<Vec<(String, String)>>,
    rsync_errors: Mutex<Vec<(String, String)>>,
    log: Mutex<Vec<String>>,
    written_files: Mutex<HashMap<String, String>>,
    file_list: Mutex<Vec<Vec<String>>>,
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
            .field("file_list", &self.file_list)
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
                spawned_cmd_exits: Mutex::new(Vec::new()),
                write_errors: Mutex::new(Vec::new()),
                rsync_errors: Mutex::new(Vec::new()),
                log: Mutex::new(Vec::new()),
                written_files: Mutex::new(HashMap::new()),
                file_list: Mutex::new(Vec::new()),
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
    pub(crate) fn set_finish_run_hook(&self, hook: Box<dyn Fn() + Send>) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner.finish_run_hook.lock().unwrap().replace(hook);
            }
            _ => unreachable!(),
        }
    }

    /// Script a spawned command exit.  cmd_contains matches the command
    /// substring, polls_before_exit is how many try_wait calls before
    /// the command returns the given exit.
    #[allow(dead_code)]
    pub(crate) fn add_spawned_cmd_exit(
        &self,
        cmd_contains: &str,
        polls_before_exit: usize,
        exit: RemoteCommandExit,
    ) {
        match self {
            RemoteRunner::Fake { inner } => {
                inner.spawned_cmd_exits.lock().unwrap().push((
                    cmd_contains.to_owned(),
                    polls_before_exit,
                    exit,
                ));
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
            RemoteRunner::Real => HashMap::new(),
        }
    }

    /// Return the lists of filter rules passed to run_rsync_filter_transfer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn filter_rule_sets(&self) -> Vec<(Vec<String>, String)> {
        match self {
            RemoteRunner::Fake { inner } => {
                let lists = inner.file_list.lock().unwrap();
                lists
                    .iter()
                    .map(|f| {
                        let includes: Vec<_> =
                            f.iter().take(f.len().saturating_sub(1)).cloned().collect();
                        let exclude = f.last().cloned().unwrap_or_default();
                        (includes, exclude)
                    })
                    .collect()
            }
            RemoteRunner::Real => Vec::new(),
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
                let mut responses = inner.responses.lock().unwrap();
                let matched_idx = responses
                    .iter()
                    .position(|resp| ssh_cmd.contains(&resp.cmd_contains));
                if let Some(idx) = matched_idx {
                    let resp = responses.remove(idx);
                    drop(responses);
                    return Ok(resp.stdout);
                }
                anyhow::bail!(
                    "no scripted response for command (did you forget add_response?), \
                     cmd was: {ssh_cmd}"
                )
            }
        }
    }

    /// Spawn a long-running remote command and return a handle for
    /// concurrent supervision.  Used for `process-run`.
    ///
    /// The caller must poll `try_wait()` on the returned handle.
    /// Do not use `.output()` — that blocks until the remote command exits.
    pub(crate) fn spawn_server_cmd(
        &self,
        host: &str,
        server_cmd: &str,
        args: &[&str],
    ) -> Result<RemoteCommandHandle> {
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
                let child = Command::new("ssh")
                    .arg("--")
                    .arg(host)
                    .arg(&full_cmd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .with_context(|| format!("failed to spawn SSH command on {host}"))?;
                Ok(RemoteCommandHandle {
                    kind: RemoteCommandHandleKind::Real { child: Some(child) },
                    spawned_cmd_contains: ssh_cmd,
                })
            }
            RemoteRunner::Fake { inner } => {
                inner.log.lock().unwrap().push(ssh_cmd.clone());
                // Check for errors first.
                for (ec, err) in inner.errors.lock().unwrap().iter() {
                    if ssh_cmd.contains(ec.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                // Find the next matching spawned command exit script.
                let mut exits = inner.spawned_cmd_exits.lock().unwrap();
                let idx = exits.iter().position(|(ec, _, _)| ssh_cmd.contains(ec));
                if let Some(idx) = idx {
                    let (_, polls, exit) = exits.remove(idx);
                    let remaining = if exit == RemoteCommandExit::Killed {
                        None
                    } else {
                        Some((polls as u64, exit))
                    };
                    let exit_cell = Arc::new(Mutex::new(None::<RemoteCommandExit>));
                    Ok(RemoteCommandHandle {
                        kind: RemoteCommandHandleKind::Fake {
                            remaining_polls: Arc::new(Mutex::new(remaining)),
                            exit: Arc::clone(&exit_cell),
                        },
                        spawned_cmd_contains: ssh_cmd,
                    })
                } else {
                    // No exit scripted — command stays running forever.
                    Ok(RemoteCommandHandle {
                        kind: RemoteCommandHandleKind::Fake {
                            remaining_polls: Arc::new(Mutex::new(None)),
                            exit: Arc::new(Mutex::new(None)),
                        },
                        spawned_cmd_contains: ssh_cmd,
                    })
                }
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

    /// Run rsync with include/exclude filter rules for pure passthrough split.
    ///
    /// Uses one rsync invocation with a constant number of include/exclude
    /// rules derived from the split pattern. The source operand in filter
    /// mode has a trailing slash so selected entries land under `<TARGET>`
    /// rather than under `<TARGET>/<SOURCE-NAME>`.
    pub(crate) fn run_rsync_filter_transfer(
        &self,
        source: &str,
        host: &str,
        remote_dir: &str,
        include_rules: &[String],
        exclude_rule: &str,
    ) -> Result<()> {
        let rsync_dest = format!("{host}:{remote_dir}/");
        let source_with_slash = format!("{source}/");

        let mut args: Vec<String> = vec![
            "--archive".to_string(),
            "--partial".to_string(),
            "--inplace".to_string(),
            "--mkpath".to_string(),
            "--protect-args".to_string(),
            "--prune-empty-dirs".to_string(),
        ];
        for rule in include_rules {
            args.push(format!("--include={rule}"));
        }
        args.push(format!("--exclude={exclude_rule}"));
        args.push("--".to_string());
        args.push(source_with_slash);
        args.push(rsync_dest);

        let rsync_cmd = args.join(" ");

        match self {
            RemoteRunner::Real => {
                let status = Command::new("rsync")
                    .args(&args)
                    .status()
                    .with_context(|| {
                        format!(
                            "failed to execute rsync filter transfer: {source} -> {host}:{remote_dir}"
                        )
                    })?;
                if !status.success() {
                    anyhow::bail!("rsync filter transfer failed: {source} -> {host}:{remote_dir}");
                }
                Ok(())
            }
            RemoteRunner::Fake { inner } => {
                if let Some(hook) = inner.rsync_hook.lock().unwrap().take() {
                    hook();
                }
                for (rc, err) in inner.rsync_errors.lock().unwrap().iter() {
                    if rsync_cmd.contains(rc.as_str()) {
                        anyhow::bail!("{err}");
                    }
                }
                inner.log.lock().unwrap().push(rsync_cmd);
                let mut all_rules: Vec<String> = include_rules.to_vec();
                all_rules.push(exclude_rule.to_string());
                inner.file_list.lock().unwrap().push(all_rules);
                Ok(())
            }
        }
    }

    /// Parse a BeginRunResponse from the given TOML string. Convenience for tests.
    pub(crate) fn parse_begin_response(toml: &str) -> Result<BeginRunResponse> {
        let resp: BeginRunResponse =
            toml::from_str(toml).with_context(|| "failed to parse begin-run response")?;
        if resp.protocol_version != purgery_core::PROTOCOL_VERSION {
            anyhow::bail!(
                "begin-run response has protocol_version {}; expected {}",
                resp.protocol_version,
                purgery_core::PROTOCOL_VERSION,
            );
        }
        purgery_core::require_compatible_purgery_version(
            &resp.purgery_version,
            "begin-run response",
        )
        .with_context(|| "incompatible purgery_version in begin-run response")?;
        Ok(resp)
    }

    /// Parse a PrepareRunResponse from the given TOML string.
    pub(crate) fn parse_prepare_response(toml: &str) -> Result<PrepareRunResponse> {
        let resp: PrepareRunResponse =
            toml::from_str(toml).with_context(|| "failed to parse prepare-run response")?;
        if resp.protocol_version != purgery_core::PROTOCOL_VERSION {
            anyhow::bail!(
                "prepare-run response has protocol_version {}; expected {}",
                resp.protocol_version,
                purgery_core::PROTOCOL_VERSION,
            );
        }
        purgery_core::require_compatible_purgery_version(
            &resp.purgery_version,
            "prepare-run response",
        )
        .with_context(|| "incompatible purgery_version in prepare-run response")?;
        Ok(resp)
    }

    /// Parse a RunStateResponse from the given TOML string.
    pub(crate) fn parse_run_state_response(toml: &str) -> Result<RunStateResponse> {
        let resp: RunStateResponse =
            toml::from_str(toml).with_context(|| "failed to parse run-state response")?;
        if resp.protocol_version != purgery_core::PROTOCOL_VERSION {
            anyhow::bail!(
                "run-state response has protocol_version {}; expected {}",
                resp.protocol_version,
                purgery_core::PROTOCOL_VERSION,
            );
        }
        purgery_core::require_compatible_purgery_version(
            &resp.purgery_version,
            "run-state response",
        )
        .with_context(|| "incompatible purgery_version in run-state response")?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_runner_returns_scripted_responses() {
        let runner = RemoteRunner::fake();
        runner.add_response(
            "begin-run",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
        runner.add_response(
            "prepare-run",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );

        let out = runner.server_cmd("host", "ps", &["begin-run"]).unwrap();
        assert_eq!(
            out,
            format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            )
        );

        let out = runner.server_cmd("host", "ps", &["prepare-run"]).unwrap();
        assert_eq!(
            out,
            format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            )
        );

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
        runner.add_response(
            "begin-run",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
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
    fn filter_rsync_argv_puts_options_before_separator() {
        let runner = RemoteRunner::fake();
        runner
            .run_rsync_filter_transfer(
                "/src",
                "host",
                "/dest",
                &[
                    "*/".to_string(),
                    "*.mp4/***".to_string(),
                    "*.mp4".to_string(),
                ],
                "*",
            )
            .unwrap();
        let log = runner.command_log();
        assert_eq!(log.len(), 1, "must be exactly one rsync process");
        let cmd = &log[0];
        let before_sep = cmd.split(" -- ").next().unwrap();
        assert!(
            before_sep.contains("--include=*/"),
            "--include=*/ must be before --"
        );
        assert!(
            before_sep.contains("--include=*.mp4/***"),
            "--include=*.mp4/*** must be before --"
        );
        assert!(
            before_sep.contains("--exclude=*"),
            "--exclude=* must be before --"
        );
        assert!(
            before_sep.contains("--prune-empty-dirs"),
            "--prune-empty-dirs must be before --"
        );
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
    fn filter_include_exclude_argv_has_no_shell_quotes() {
        let runner = RemoteRunner::fake();
        runner
            .run_rsync_filter_transfer(
                "/src",
                "host",
                "/dest",
                &[
                    "*/".to_string(),
                    "*.mp4/***".to_string(),
                    "*.mp4".to_string(),
                ],
                "*",
            )
            .unwrap();
        let rule_sets = runner.filter_rule_sets();
        assert_eq!(rule_sets.len(), 1);
        let (includes, exclude) = &rule_sets[0];
        for rule in includes.iter().chain(std::iter::once(exclude)) {
            assert!(
                !rule.contains('\''),
                "include/exclude rules must not contain shell quote characters, got: {rule}"
            );
        }
    }

    #[test]
    fn pure_passthrough_split_uses_one_rsync_no_ssh() {
        let runner = RemoteRunner::fake();
        runner
            .run_rsync_filter_transfer(
                "/src",
                "host",
                "/dest",
                &[
                    "*/".to_string(),
                    "*.mp4/***".to_string(),
                    "*.mp4".to_string(),
                ],
                "*",
            )
            .unwrap();
        let log = runner.command_log();
        assert_eq!(log.len(), 1, "pure passthrough split must use one rsync");
        assert!(!log[0].contains("ssh"), "must be an rsync command, not ssh");
    }

    #[test]
    fn filter_transfer_records_rules() {
        let runner = RemoteRunner::fake();
        let includes = vec![
            "*/".to_string(),
            "*.mp4/***".to_string(),
            "*.mp4".to_string(),
        ];
        runner
            .run_rsync_filter_transfer("/src", "host", "/dest", &includes, "*")
            .unwrap();
        let rule_sets = runner.filter_rule_sets();
        assert_eq!(rule_sets.len(), 1, "must record exactly one rule set");
        let (recorded_includes, recorded_exclude) = &rule_sets[0];
        assert_eq!(*recorded_includes, includes);
        assert_eq!(*recorded_exclude, "*".to_string());
    }

    #[test]
    fn wait_returns_success_for_scripted_exit() {
        let runner = RemoteRunner::fake();
        runner.add_spawned_cmd_exit("test-cmd", 2, RemoteCommandExit::Success);
        let mut handle = runner
            .spawn_server_cmd("host", "ps", &["test-cmd"])
            .unwrap();
        // wait() must block until the exit is available (after 2 poll calls).
        let exit = handle.wait().unwrap();
        assert_eq!(exit, RemoteCommandExit::Success);
    }

    #[test]
    fn wait_returns_transport_failure() {
        let runner = RemoteRunner::fake();
        runner.add_spawned_cmd_exit(
            "test-cmd",
            0,
            RemoteCommandExit::TransportFailure {
                exit_code: Some(255),
                details: "connection refused".to_string(),
            },
        );
        let mut handle = runner
            .spawn_server_cmd("host", "ps", &["test-cmd"])
            .unwrap();
        let exit = handle.wait().unwrap();
        assert_eq!(
            exit,
            RemoteCommandExit::TransportFailure {
                exit_code: Some(255),
                details: "connection refused".to_string(),
            }
        );
    }

    #[test]
    fn wait_returns_remote_failure() {
        let runner = RemoteRunner::fake();
        runner.add_spawned_cmd_exit(
            "test-cmd",
            1,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "error: something failed".to_string(),
            },
        );
        let mut handle = runner
            .spawn_server_cmd("host", "ps", &["test-cmd"])
            .unwrap();
        let exit = handle.wait().unwrap();
        assert_eq!(
            exit,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "error: something failed".to_string(),
            }
        );
    }

    #[test]
    fn wait_returns_killed_for_unscripted_command() {
        let runner = RemoteRunner::fake();
        // No scripted exit for this command — wait() must return Killed immediately.
        let mut handle = runner
            .spawn_server_cmd("host", "ps", &["unknown-cmd"])
            .unwrap();
        let exit = handle.wait().unwrap();
        assert_eq!(exit, RemoteCommandExit::Killed);
    }
}
