# Purgery Protocol

## Lifecycle

```
client: local checks (config, executables)
client: generate run ID, build manifest
client: begin-run over SSH → server creates incoming directory, returns paths
client: validate response envelope
client: write run.toml + manifest.toml to incoming dir
client: rsync trees into incoming/files/ without delete
client: finish-run over SSH → server moves incoming → ready
server: recover interrupted processing runs, then claim ready → processing
server: validate config, manifest, envelope
server: for each entry, validate kind; overlay directories/symlinks; postprocess regular files
server: atomically write status.toml, move processing to done or failed
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
| `process-once` | Validate config, run GC, recover processing runs, then process ready runs |

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
| `failed` | Processing completed with no entries imported, or run-level error |

## Run states

The `state` field in `status.toml`:

| State | Meaning |
|-------|---------|
| `done` | All filesystem entries imported successfully |
| `partial` | Some entries imported, some failed or skipped |
| `failed` | No entries imported (all failed/skipped, or run-level error) |

## Status format

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/video.mp4"
relative_path = "video.mp4"
status = "imported"
final_paths = ["laptop/videos/video.mp4"]
postprocess = ["compress-video"]

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/broken.mp4"
relative_path = "broken.mp4"
status = "failed"
error = "staged file not found"
```

Per-entry status values: `imported`, `failed`, `skipped`. Status records are serialized under `[[entries]]`.

## Manifest format

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
sha256 = "abc123..."
```

The server rejects a manifest before import if multiple entries resolve to the same final path. Manifest `kind` is `directory`, `regular_file`, or `symlink`. Regular files carry `size`, `mtime_ns`, and optional `sha256`; symlinks carry a literal `link_target`; directories need no identity payload. Parent directories precede descendants. Staged validation uses `symlink_metadata` and never follows symlinks.

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

## Expected output restrictions

Expected output names in postprocess step definitions are limited to file-name patterns. The following placeholders are allowed:

- `{file_name}` — input file name with extension
- `{file_stem}` — input file name without extension
- `{stem}` — deprecated alias for `{file_stem}`

The following are **forbidden** in expected outputs (but remain allowed in `args`):

- `{input}` — full work-area input path
- `{parent}` — work-area parent directory

Additional restrictions: non-empty, not `.`, not `..`, not absolute, no `/` or `\`.

The server validates expected output names at boot time (`purgery-server check`) and at pattern resolution time.

## Lease validation on `finish-run`

When `finish-run` is called, the server reads `lease.toml` from the incoming directory and validates:

- `protocol_version == 1`
- `lease.nickname` matches the command nickname
- `lease.run_id` matches the command run ID
- The lease has not expired

If any check fails, `finish-run` rejects the transition with a clear error message.

## Crash recovery and repeated imports

`process-once` scans `processing/` as well as `ready/`. Missing processing status is replayed from staged files; valid processing status completes its pending terminal move; malformed processing status becomes a clear failure. The staged tree is replayed through idempotent per-entry directory, regular-file, and symlink commits. Existing directories merge without deleting unrelated descendants, final-storage symlinks are never followed as directories, and terminal success is published only after every entry completes. This provides replayable convergence rather than an all-or-nothing tree transaction. See [Crash Safety and Idempotent Imports](design/crash-safety-and-idempotence.md).
