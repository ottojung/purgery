# Purgery

A Rust client/server file sync and postprocessing tool.

Clients upload files to a server. The server validates runs, applies optional postprocessing (e.g., video compression), moves files into final storage, and writes a status file. The client then deletes only the local files confirmed as successfully imported.

## Lifecycle

1. Client syncs local files into a per-run server staging directory via `rsync --recursive --partial --archive`.
2. Client atomically moves the run from `incoming/` to `ready/`.
3. Server claims the run (atomic rename `ready/` → `processing/`).
4. Server validates the run's config (including postprocess regexes), manifest, and envelope.
5. For each file, the server validates staged path identity (size/SHA-256), rejects symlinks, copies to a work area namespaced by sync mapping destination (`<work> / <to_path> / <relative_path>`), applies postprocessing, then commits each output via a same-directory temp file (`.purgery-commit.<run_id>.<filename>.tmp`) followed by atomic `rename`.
6. Server writes `status.toml` atomically.
7. Client reads status and deletes only unchanged, confirmed-imported local files.

## Import / Commit Semantics

### Temp-file commit

Outputs are never copied directly to their final user-visible path. Instead each committed output goes through:

```
work output → final parent dir / .purgery-commit.<run_id>.<filename>.tmp → rename → final path
```

The temp file is on the same filesystem as the final path, so the rename is atomic against readers. Temp files are cleaned up after a successful commit.

### Conflict policy: fail-if-exists

Purgery does not overwrite existing final files. If any final output path already exists before commit, the file is marked as `failed` in the status. This is not intended as a defense against hostile concurrent writers; it is a conservative default.

### `final_paths` (plural)

Status entries use `final_paths` — a list of all committed paths. A single-output import produces one entry. Postprocessing (e.g., `compress-video`) may produce multiple outputs (original + compressed). Example:

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

Per-file failures produce individual `FileStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining files. Only truly catastrophic errors (unreadable config, invalid regex, unparseable manifest, envelope mismatch) abort the entire run with a single run-level error.

Examples of per-file failures:
- final output already exists
- staged path mismatch (manifest `staged_path` does not match `files / <to_path> / <relative_path>`)
- staged file missing
- staged file is a symlink
- staged file size/SHA mismatch
- postprocessing command fails
- expected compressed output missing
- temp-file commit fails

### Work area

Server creates a hidden work area at `<root>/.purgery-work/<nickname>/<run_id>/`. Files are copied into subdirectories mirroring the destination structure: `<work_area>/<to_path>/<relative_path>`.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

### Staged path validation

The server derives the expected staged path as `files / <sync.to_path> / <relative_path>` and requires `manifest.staged_path == expected`. A mismatch marks the file as failed. This prevents a manifest from claiming a file belongs to one sync mapping while pointing to a different staged file.

### Staged symlink rejection

Before copying a staged file into the work area, the server checks the staged path with `symlink_metadata`. If it is a symlink, the file is marked as failed. Purgery imports regular files listed in the manifest, not symlinks.

### Postprocess regex validation

All client postprocess regexes are validated before any file processing. If any regex is invalid, the run is aborted with a run-level `Failed` status. Originals are not imported when a compression regex fails to compile.

### Compress-video commit

For `keep_original = true`: commit both the original and the compressed output (`video.Z.webm`). Status records two `final_paths`.

For `keep_original = false`: commit only the compressed output. Status records one `final_path`.

If any required output is missing or cannot be committed, the file is marked as failed and the client local source is not deleted.

### Run states

| Run state | Meaning |
|-----------|---------|
| `done`    | all files imported successfully |
| `partial` | some files imported, some failed or skipped |
| `failed`  | no files imported (all failed or skipped, or run-level error) |

## Binaries

- **purgery-client** — runs on user machines; syncs files and cleans up on confirmation.
- **purgery-server** — runs on the server; processes ready runs, postprocesses, and writes status.

## Example Config

### Client (`client.toml`)

```toml
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/universe/tmp/purgery"

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
kind = "compress-video"
program = "my-compress-video"
keep_original = true
```

## Safety Rule

Local files are deleted only after:

1. The server's `status.toml` is valid.
2. The file's status is `imported`.
3. The local file still matches the uploaded identity (size, mtime, and optional SHA-256).
4. The sync mapping has `delete_after_import = true`.

## Non-Goals (Initial Version)

- Network daemon protocol or HTTP API.
- Multi-user authorization beyond SSH/filesystem permissions.
- Arbitrary client-defined shell commands.
- Bidirectional sync.
- `rsync --delete`.
- Automatic conflict resolution.
- Distributed locking beyond atomic filesystem renames.
