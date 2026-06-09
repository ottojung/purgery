# Purgery

A Rust client/server file sync and postprocessing tool.

Clients upload files to a server. The server validates runs, applies optional postprocessing (e.g., video compression), moves files into final storage, and writes a status file. The client then deletes only the local files confirmed as successfully imported.

## Lifecycle

1. Client runs local checks (ssh, rsync, server reachability), then calls `purgery-server begin-run` over SSH.
2. Client validates the `begin-run` response envelope (protocol_version, nickname, run_id, all paths absolute).
3. Client builds manifest and writes `run.toml` + `manifest.toml` to the server's incoming directory.
4. Client rsyncs files into the server's files directory, namespaced by sync mapping destination.
5. Client runs `purgery-server finish-run` to atomically move `incoming/` → `ready/`.
6. Server runs server checks, then claims the run (atomic rename `ready/` → `processing/`).
7. Server builds a `RunPlan`: compiles all regexes once, resolves step references. If invalid, the run is rejected before any file import.
8. For each file, the server validates staged path identity (size/SHA-256), rejects symlinks, copies to a work area namespaced by sync mapping destination, applies postprocessing via subprocess using the precompiled plan, then commits each output via a same-directory temp file followed by atomic `rename`. Failed files produce per-file status entries; remaining files continue processing.
9. Server writes `status.toml` atomically.
10. Client polls `purgery-server status` until the run completes.
11. Client verifies `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`, then deletes only unchanged, confirmed-imported local files.

## Import / Commit Semantics

### Temp-file commit

Outputs are never copied directly to their final user-visible path. Each committed output goes through:

```
work output → final parent dir / .purgery-commit.<run_id>.<filename>.tmp → rename → final path
```

The temp file is on the same filesystem as the final path, so the rename is atomic against readers. Temp files are cleaned up after a successful commit.

### Conflict policy: fail-if-exists

Purgery does not overwrite existing final files. If any final output path already exists before commit, the file is marked as `failed` in the status. This is not intended as a defense against hostile concurrent writers; it is a conservative default.

### Multi-output preflight and rollback

Before committing any output for a file, all final output paths are derived and prechecked (none exists, no symlinks in path, parent directories creatable). Commits proceed in order. If a later output commit fails, outputs already committed during this file's import are rolled back (removed).

### `final_paths` (plural)

Status entries use `final_paths` — a list of all committed paths relative to the server root. A single-output import produces one entry. Postprocessing (e.g., `compress-video`) may produce multiple outputs (original + compressed). Example:

```toml
[[files]]
sync_name = "videos"
local_path = "/home/user/Videos/video.mp4"
relative_path = "video.mp4"
status = "imported"
final_paths = [
  "laptop/videos/video.mp4",
  "laptop/videos/video.Z.webm",
]
postprocess = ["compress-video"]
```

For a failed file, `final_paths` is empty:

```toml
[[files]]
sync_name = "videos"
local_path = "/home/user/Videos/missing.mp4"
relative_path = "missing.mp4"
status = "failed"
error = "staged file not found"
```

### Per-file errors

Per-file failures produce individual `FileStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining files. Only truly catastrophic errors (unreadable run config, invalid regex, missing step reference, unparseable manifest, envelope mismatch) abort the entire run.

### Work area

Server creates a hidden work area at `<root>/.purgery-work/<nickname>/<run_id>/`. Files are copied into subdirectories mirroring the destination structure: `<work_area>/<to_path>/<relative_path>`.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

### Run plan validation

Before processing any files, the server builds a `RunPlan` that compiles all postprocess regexes once and resolves every referenced step against the server config. If any regex is invalid or any step is missing on the server, the run is rejected with a run-level `Failed` status before any file is imported. File processing uses the precompiled plan and never recompiles regexes.

### Malformed status handling

`purgery-server status` returns the status file if it exists and parses correctly. If a `status.toml` exists but is malformed (e.g., invalid TOML or missing required fields), the command returns a parse error rather than silently skipping it.

## Run states

| Run state | Meaning |
|-----------|---------|
| `done`    | all files imported successfully |
| `partial` | some files imported, some failed or skipped |
| `failed`  | no files imported (all failed or skipped, or run-level error) |

## Client/Server Protocol

The client communicates with the server over SSH by invoking `purgery-server` subcommands:

| Subcommand | Purpose |
|------------|---------|
| `begin-run --nickname <n> --run-id <id>` | Creates incoming directory, prints machine-readable TOML with server-derived paths |
| `finish-run --nickname <n> --run-id <id>` | Atomically moves run from `incoming` to `ready` |
| `status --nickname <n> --run-id <id>` | Returns `status.toml` from `done` or `failed` |
| `check` | Validates server config and postprocess dependencies |
| `process-once` | Processes one batch of ready runs |

Config discovery for server commands:

1. `--config PATH` (explicit)
2. `$PURGERY_CONFIG` environment variable
3. `~/.config/purgery/server.toml`
4. `/etc/purgery/server.toml`

The client never constructs server paths from local configuration.

## Run config vs Client config

The uploaded run configuration (`run.toml`) is a subset of the local client config. It includes:

- `nickname`
- sync mappings (name + `to` path only)
- postprocess rules

It does **not** include:

- server host or command
- server `purgery_root`
- local source `from` paths

This separation keeps server topology server-owned.

## Postprocess steps

Server-side postprocess step kind is `"subprocess"`. Steps are defined with:

```toml
[postprocess.steps.compress-video]
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.Z.webm"]
keep_original = true
```

Supported placeholders in `args` and `expected_outputs`:

| Placeholder | Resolves to |
|-------------|-------------|
| `{input}` | Absolute work-area input path |
| `{parent}` | Work-area parent directory |
| `{file_name}` | Input file name with extension |
| `{file_stem}` | Input file name without extension |
| `{stem}` | Deprecated alias for `{file_stem}` |

A subprocess step must produce at least one committed output. If `keep_original = false`, then `expected_outputs` must be non-empty. This is validated at server boot time.

The client references steps by name only:

```toml
[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
```

## Boot-time checks

Both binaries support a `check` subcommand:

```sh
purgery-client check --config client.toml
purgery-server check --config server.toml
```

Client checks: `ssh` and `rsync` executables are resolved (via `resolve_executable`), server is reachable via `purgery-server check`.

Server checks: `root` and `purgery_root` are accessible, all postprocess programs are resolved (absolute paths must exist and be executable; relative names found in PATH and must be executable), config is internally valid.

Normal operations run the same checks before mutating state:

- `purgery-client sync-and-cleanup` calls `client_check` before `begin-run`.
- `purgery-server process-once` calls `server_check` before scanning ready runs.
- `purgery-server begin-run` and `purgery-server finish-run` call `server_check` before mutating state.

### `begin-run` single-use safety

`begin-run` fails if the run ID already exists in any phase (incoming, ready, processing, done, failed). A run ID is single-use per nickname.

`finish-run` fails if the incoming directory does not exist or the ready directory already exists.

### `server.command` trust model

The client's `[server].command` value is a trusted shell command prefix executed on the remote host via SSH. Purgery appends shell-escaped arguments. This is not intended to accept untrusted input.

## Executable resolution

Executable resolution (`purgery_core::resolve_executable`) follows these rules:

- **Absolute path**: must exist, must be executable (file type is symlink or Unix executable bit is set).
- **Relative name**: searched in `PATH`; first executable candidate wins.

This is used for client `ssh`, `rsync`, and server postprocess `program` values.

## Status envelope verification

Before deleting any local file, the client verifies:

- `status.nickname == manifest.nickname`
- `status.run_id == manifest.run_id`

If either mismatches, cleanup is aborted and nothing is deleted.

## Binaries

- **purgery-client** — runs on user machines; syncs files and cleans up on confirmation. Supports `sync-and-cleanup` and `check`.
- **purgery-server** — runs on the server. Supports `process-once`, `begin-run`, `finish-run`, `status`, and `check`.

## Example Config

### Client (`client.toml`)

```toml
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/vitalik/Videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = '^videos/.*\.(mp4|mov|mkv|webm)$'
steps = ["compress-video"]
```

### Server (`server.toml`)

```toml
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
state_dir = "/var/lib/purgery"
log_dir = "/var/log/purgery"

[postprocess]
max_parallel_jobs = 1

[postprocess.steps.compress-video]
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.Z.webm"]
keep_original = true
```

## Safety Rule

Local files are deleted only after:

1. The server's `status.toml` is valid.
2. `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`.
3. The file's status is `imported`.
4. The local file still matches the uploaded identity (size, mtime, and optional SHA-256).
5. The sync mapping has `delete_after_import = true`.

## Non-Goals (Initial Version)

- Network daemon protocol or HTTP API.
- Multi-user authorization beyond SSH/filesystem permissions.
- Arbitrary client-defined shell commands.
- Bidirectional sync.
- `rsync --delete`.
- Automatic conflict resolution.
- Distributed locking beyond atomic filesystem renames.
