# Purgery Protocol

## Lifecycle

```
client: local checks (config, executables)
client: generate run ID, build manifest
client: begin-run over SSH → server creates incoming directory, returns paths
client: validate response envelope
client: write run.toml + manifest.toml to incoming dir
client: rsync files into incoming/files/
client: finish-run over SSH → server moves incoming → ready
server: claim run by renaming ready → processing
server: validate config, manifest, envelope
server: for each file, copy to work area, apply postprocessing, commit outputs
server: write status.toml, move to done or failed
client: poll server status, verify envelope, clean up confirmed local files
```

## Server subcommands

| Subcommand | Purpose |
|------------|---------|
| `begin-run --nickname <n> --run-id <id>` | Create incoming directory, write lease file, print TOML response with server paths |
| `finish-run --nickname <n> --run-id <id>` | Move run from `incoming` to `ready` |
| `status --nickname <n> --run-id <id>` | Return `status.toml` from `done` or `failed` |
| `heartbeat-run --nickname <n> --run-id <id>` | Update lease file for an incoming run |
| `check` | Validate config and postprocess dependencies (side-effect-free) |
| `bootstrap` | Create `root` and `purgery_root` directories |
| `gc` | Run garbage collection on expired incoming runs |
| `process-once` | Validate config, run GC, then process one batch of ready runs |

## Run phases

Runs move through these phases during their lifecycle:

```text
incoming → ready → processing → done
                                 failed
```

| Phase | Description |
|-------|-------------|
| `incoming` | Client is uploading files |
| `ready` | Upload complete, waiting for server processing |
| `processing` | Server is actively processing the run |
| `done` | Processing completed (all or partial success) |
| `failed` | Processing completed with no files imported, or run-level error |

## Run states

The `state` field in `status.toml`:

| State | Meaning |
|-------|---------|
| `done` | All files imported successfully |
| `partial` | Some files imported, some failed or skipped |
| `failed` | No files imported (all failed/skipped, or run-level error) |

## Status format

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[files]]
sync_name = "videos"
local_path = "/home/user/Videos/video.mp4"
relative_path = "video.mp4"
status = "imported"
final_paths = ["laptop/videos/video.mp4"]
postprocess = ["compress-video"]

[[files]]
sync_name = "videos"
local_path = "/home/user/Videos/broken.mp4"
relative_path = "broken.mp4"
status = "failed"
error = "staged file not found"
```

Per-file status values: `imported`, `failed`, `skipped`.

## Manifest format

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[files]]
sync_name = "videos"
local_path = "/home/user/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
sha256 = "abc123..."
```

## `begin-run` response

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
incoming_dir = "/tmp/purgery/laptop/incoming/01ARZ..."
files_dir = "/tmp/purgery/laptop/incoming/01ARZ.../files"
run_config_path = "/tmp/purgery/laptop/incoming/01ARZ.../run.toml"
manifest_path = "/tmp/purgery/laptop/incoming/01ARZ.../manifest.toml"
heartbeat_interval_secs = 60
```
