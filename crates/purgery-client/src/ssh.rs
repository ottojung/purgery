use anyhow::{Context, Result};
use purgery_core::shell_escape;

pub(crate) fn ssh_run(host: &str, cmd: &str) -> Result<String> {
    let output = std::process::Command::new("ssh")
        .arg("--")
        .arg(host)
        .arg(cmd)
        .output()
        .with_context(|| format!("failed to execute SSH command on {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn server_cmd(host: &str, server_command: &str, args: &[&str]) -> Result<String> {
    let full_cmd = {
        let mut cmd = server_command.to_owned();
        for a in args {
            cmd.push(' ');
            cmd.push_str(&shell_escape(a));
        }
        cmd
    };
    ssh_run(host, &full_cmd)
}

pub(crate) fn server_cmd_with_stdin(
    host: &str,
    server_command: &str,
    args: &[&str],
    stdin_content: &str,
) -> Result<String> {
    let full_cmd = {
        let mut cmd = server_command.to_owned();
        for a in args {
            cmd.push(' ');
            cmd.push_str(&shell_escape(a));
        }
        cmd
    };
    let mut child = std::process::Command::new("ssh")
        .arg("--")
        .arg(host)
        .arg(&full_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn SSH command to {host}"))?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_content.as_bytes())
            .with_context(|| "failed to write stdin content")?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for SSH command on {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[allow(dead_code)]
pub(crate) fn read_remote_file(host: &str, path: &str) -> Result<String> {
    ssh_run(host, &format!("cat {}", shell_escape(path)))
}

pub(crate) fn write_remote_file(host: &str, path: &str, content: &str) -> Result<()> {
    let remote_cmd = format!("cat > {}", purgery_core::shell_escape(path));
    let mut child = std::process::Command::new("ssh")
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
