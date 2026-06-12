# Crash safety and idempotence

## Client cleanup state

The client stores durable cleanup records under `--state-dir`, defaulting to `$XDG_STATE_HOME/purgery` or `~/.local/state/purgery`. Pending cleanup records are replayed at the start of every invocation.

Each cleanup entry records the local path, relative path, entry kind, and identity. Cleanup is idempotent: missing entries are marked complete, unchanged authorized entries are removed, and changed entries remain untouched.

Authority depends on the mode:

- direct passthrough: successful rsync confirms every recorded transfer entry;
- postprocess: a valid terminal server status confirms only entries marked `imported`.

A successful upload to postprocess staging never authorizes cleanup.

## Server phases

Postprocess runs move through `incoming -> ready -> processing -> done|failed`. `prepare-run` validates the plan while it is still incoming. `finish-run` is the only transition to ready.

Processing uses staged sources and a run-local work area. Status files are published atomically before terminal phase publication. Recovery resumes processing runs from staged data and filesystem state, so repeated commits converge on the requested destination.

## Storage separation

`work_dir` owns incoming files, phase directories, manifests, leases, progress, status, and work areas. Final entries are committed only beneath the destination recorded in `run.toml`, whether that destination is absolute or relative.
