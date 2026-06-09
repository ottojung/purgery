# Purgery

One-way file import with optional postprocessing and safe local cleanup.

Purgery collects files from local directories, imports them into managed storage on a server, optionally transforms them during import, and can safely remove local copies after the server confirms the import.

Use Purgery when you want to regularly move photos, videos, recordings, or other generated files from devices into a central archive — possibly compressing or converting them on the way — without risking deletion before the import is confirmed.

## Non-goals

Purgery is not bidirectional sync, not a Dropbox/Syncthing replacement, not a network daemon, not a multi-user authorization system, not a remote shell execution framework, and not an automatic conflict-resolution system. It is intentionally a one-way import pipeline.

## Quick start

### Server

```sh
# Create root directories
purgery-server bootstrap --config server.toml

# Verify configuration and dependencies
purgery-server check --config server.toml

# Run one batch of imports
purgery-server process-once --config server.toml
```

### Client

```sh
# Verify local executables and configuration (no SSH)
purgery-client check --config client.toml

# Run a full sync: upload, wait for processing, clean up confirmed imports
purgery-client sync-and-cleanup --config client.toml
```

## How it works

1. The client collects files from configured local directories and builds a manifest.
2. The client uploads files to a server staging area over SSH via rsync.
3. The server validates the run, optionally applies postprocessing steps to matching files, and moves imported files into final storage.
4. The server writes a status file describing what was imported and what failed.
5. The client reads the status and, for sync mappings with cleanup enabled, removes only confirmed unchanged local originals.

## Configuration

Minimal server config (`server.toml`):

```toml
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
```

Minimal client config (`client.toml`):

```toml
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
```

Full config reference: [docs/config.md](docs/config.md)

## Postprocessing

Postprocessing is defined on the server. Clients request named steps by rule; they do not upload arbitrary commands.

```toml
# server.toml
[postprocess.steps.compress-video]
kind = "subprocess"
program = "/usr/local/bin/compress"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.compressed.webm"]
keep_original = true
```

```toml
# client.toml
[[postprocess.rules]]
match = '^videos/.*\.(mp4|mov|mkv|webm)$'
steps = ["compress-video"]
```

## Safety model

Purgery is conservative about data loss:

- Cleanup is opt-in per sync mapping (`delete_after_import = true`).
- The client deletes local files only after the server confirms the import in a valid status file whose nickname and run ID match the original upload.
- Before deleting, the client verifies the local file still matches its uploaded identity (size, mtime, optional SHA-256).
- The server never overwrites existing files. If a final output path exists, the file is marked failed.

## More documentation

- [Config reference](docs/config.md) — server, client, postprocess, run config
- [Protocol](docs/protocol.md) — lifecycle, subcommands, run states, status format
- [Operations](docs/operations.md) — bootstrap, check, GC, heartbeat, leases
- [Import semantics](docs/design/import-semantics.md) — commit model, work areas, rollback, safety rules
