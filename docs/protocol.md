# Purgery Protocol

## Lifecycle

### Path A: Pure passthrough (no --postprocess)

```
client: validate args
client: begin-run over SSH -> server creates incoming directory, returns paths
client: direct unfiltered rsync to destination
client: if --delete-after-import:
          write durable cleanup state atomically
          remove confirmed local originals
client: finish-run over SSH -> server moves incoming -> ready
```

### Path B: Postprocess (with --postprocess)

```
client: validate args (#postprocess requires --delete-after-import)
client: walk source tree, build manifest with entry classification
client: generate run ID
client: begin-run over SSH -> server creates incoming directory, returns paths
client: write run.toml + manifest.toml to server incoming dir
client: rsync to server staging area (files/)
client: prepare-run over SSH -> server validates plan
client: persist local run state as upload_complete_finish_pending
client: finish-run over SSH -> server moves incoming -> ready
client: persist local state as waiting_for_terminal_state
client: wait using run-state; retry only on ready/processing
client: on terminal: read status
client: remove confirmed local originals (server-confirmed cleanup)
client: persist local state as cleanup_complete
```

## Server subcommands

| Command | Side effects | Returns |
|---------|-------------|---------|
| `begin-run --nickname N --run-id R` | Creates `incoming/R/` with lease, files/ dir | `BeginRunResponse` TOML |
| `prepare-run --nickname N --run-id R` | Validates manifest, generates transfer plan | `PrepareRunResponse` TOML |
| `finish-run --nickname N --run-id R` | Moves run from incoming to ready | (none) |
| `heartbeat-run --nickname N --run-id R` | Extends incoming lease | (none) |
| `run-state --nickname N --run-id R` | None | `RunStateResponse` TOML |
| `status --nickname N --run-id R` | None | `RunStatus` TOML |
| `process-once` | GC + recover + process one ready run | (none) |
| `check` | None | (none) |
| `gc` | Collects expired runs | (none) |

## Run phases

```
incoming → ready → processing → done
                            ↘ failed
```

A run moves from incoming to ready when the client calls `finish-run`. The server moves ready runs to processing internally during `process-once`. On completion, runs move to done or failed.

All protocol output goes to stdout as TOML. Logs go to stderr.

## BeginRunResponse

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
incoming_dir = "/var/lib/purgery/work/laptop/incoming/01ARZ3NDEKTSV4RRFFQ69G5FAV"
files_dir = "/var/lib/purgery/work/laptop/incoming/01ARZ3NDEKTSV4RRFFQ69G5FAV/files"
run_config_path = "/var/lib/purgery/work/laptop/incoming/01ARZ3NDEKTSV4RRFFQ69G5FAV/run.toml"
manifest_path = "/var/lib/purgery/work/laptop/incoming/01ARZ3NDEKTSV4RRFFQ69G5FAV/manifest.toml"
heartbeat_interval_secs = 60
```

## Run config (run.toml)

```toml
nickname = "laptop"
to = "/universe/synced/videos"
delete_after_import = true
```

The `to` field is the destination path. Relative destinations are accepted. Final archive paths are computed as `{to}/{relative_entry_path}`.

## Manifest (manifest.toml)

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
local_path = "/home/user/Videos/test.mp4"
staged_path = "files/test.mp4"
relative_path = "test.mp4"
kind = "regular_file"
mode = "postprocess"
size = 1048576
mtime_ns = 1700000000000000000
sha256 = "abc123..."
postprocess_steps = ["compress-video"]
```

Staged paths use the format `files/<relative-path>`.

## Status (status.toml)

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
local_path = "/home/user/Videos/test.mp4"
relative_path = "test.mp4"
status = "imported"
final_paths = ["univ/videos/test.mp4"]
postprocess = ["compress-video"]
```

`final_paths` entries are `<destination>/<relative_path>`. The nickname is operational metadata and does not appear in final_paths.

## RunStateResponse

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
phase = "processing"
terminal = false
message = "processing entry 3/10"
updated_at_unix_secs = 1234567890
observed_at_unix_secs = 1234567891
```

Phases: `incoming`, `ready`, `processing`, `done`, `failed`, `not_found`. Terminal phases: `done`, `failed`, `not_found`.

## Client run state persistence

The client persists per-run state under `{state_dir}/runs/{nickname}-{run_id}/state.toml`. This enables crash-safe resume of waiting and cleanup.

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
manifest = "..."
run_config = "..."
phase = "waiting_for_terminal_state"
```

Phases: `upload_complete_finish_pending`, `waiting_for_terminal_state`, `terminal_status_seen`, `cleanup_complete`, `abandoned`, `corrupt`.
