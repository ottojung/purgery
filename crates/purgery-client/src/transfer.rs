use anyhow::{Context, Result};
use purgery_core::build_rsync_args;
use std::process::Command;

pub(crate) fn run_rsync(source: &str, host: &str, remote_dir: &str) -> Result<()> {
    let rsync_dest = format!("{host}:{remote_dir}/");
    let rsync_args = build_rsync_args(source, &rsync_dest);
    let status = Command::new("rsync")
        .args(&rsync_args)
        .status()
        .with_context(|| format!("failed to execute rsync: {source} -> {host}:{remote_dir}"))?;
    if !status.success() {
        anyhow::bail!("rsync failed: {source} -> {host}:{remote_dir}");
    }
    Ok(())
}
