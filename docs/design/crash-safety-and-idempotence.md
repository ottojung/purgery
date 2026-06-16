# Crash safety and idempotence

## Client cleanup state

The client stores durable cleanup records under `--state-dir`, defaulting to `$XDG_STATE_HOME/purgery` or `~/.local/state/purgery`. Pending cleanup records are replayed at the start of every invocation.

Each cleanup entry records the local path, relative path, entry kind, and identity. For a directory source, descendant identities are also recorded for safe recursive deletion, but the manifest and server run describe one logical entry. Cleanup is idempotent: missing entries are marked complete, unchanged authorized entries are removed, and changed entries remain untouched.

Authority depends on the mode:

- direct passthrough: successful rsync confirms the transfer;
- transform: a valid terminal server status confirms only entries marked `imported`.

A successful upload to transform staging never authorizes cleanup.

## Server phases

Transform runs move through `incoming -> ready -> processing -> done|failed`. `prepare-run` validates the plan while it is still incoming. `finish-run` is the only transition to ready.

Processing uses staged sources and a run-local work area. The server invokes transform subprocesses with `{target_directory}` pointing at the final destination parent. Transform programs are trusted to place outputs directly into `{target_directory}`; the server checks declared `expected_outputs` exist after the transform. Status files are published atomically before terminal phase publication. Recovery resumes processing runs from staged data and filesystem state.

Transformed inputs are consumed by the transform flow and are never committed as final outputs. A transform produces exactly the declared `expected_outputs`. Transform programs may intentionally perform deletion-only or no-output imports when `expected_outputs = []`. In this case, successful subprocess exit is sufficient and no final destination checks are performed.

## Storage separation

`work_dir` owns incoming files, phase directories, manifests, leases, progress, status, and work areas. Final entries are committed only beneath the destination recorded in `run.toml`, whether that destination is absolute or relative. For non-transform entries, the server moves the work entry to its final path. For transform entries, the transform program places outputs directly in `{target_directory}`; the server does not move or commit transform outputs.
