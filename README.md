# Purgery

A Rust client/server file sync and postprocessing tool.

Clients upload files to a server over SSH via `rsync`. The server validates runs, applies optional postprocessing (e.g., video compression), moves files into final storage, and writes a status file. The client then deletes only the local files confirmed as successfully imported.

## Lifecycle

1. Client syncs local files into a per-run server staging directory via `rsync --recursive --partial --archive`.
2. Client atomically moves the run from `incoming/` to `ready/`.
3. Server claims the run (atomic rename `ready/` → `processing/`).
4. Server validates the run's config and manifest.
5. Server moves matching files to final storage, applying postprocessing steps.
6. Server writes `status.toml` atomically.
7. Client reads status and deletes only unchanged, confirmed-imported local files.

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
kind = "builtin"
command = "my-compress-video"
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
