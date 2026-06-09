# Purgery Protocol

## Lifecycle

```
client: local checks (config, executables)
client: generate run ID, build manifest with entry classification
client: begin-run over SSH -> server creates incoming directory, returns paths
client: validate begin-run response envelope
client: write run.toml + manifest.toml to incoming dir
client: prepare-run over SSH -> server validates plan, returns transfer destinations
client: for each sync group:
          run passthrough rsync directly to final storage (non-postprocess entries)
          write passthrough receipt after successful passthrough rsync
          immediately cleanup eligible passthrough regular files
          run purgatory rsync to incoming/files (postprocess entries)
client: finish-run over SSH -> server moves incoming -> ready
server: claim run by renaming ready -> processing
server: process postprocess entries (verify staged content, prepare work area, run subprocesses, commit outputs)
server: publish status for all entries (passthrough via receipt, postprocess via processing, covered as skipped)
server: write status.toml, move to done or failed
client: poll status, verify envelope, cleanup postprocessed regular files as soon as imported
```

## Server subcommands

| Subcommand | Purpose |
|------------|---------|
| `begin-run --nickname <n> --run-id <id>` | Create incoming directory, write lease file, print TOML response with server paths |
| `prepare-run --nickname <n> --run-id <id>` | Validate run config and manifest, return passthrough and purgatory transfer destinations |
| `finish-run --nickname <n> --run-id <id>` | Move run from `incoming` to `ready` |
| `status --nickname <n> --run-id <id>` | Return `status.toml` from `done` or `failed` |
| `heartbeat-run --nickname <n> --run-id <id>` | Update lease file for an incoming run |
| `check` | Validate config and dependencies (side-effect-free) |
| `bootstrap` | Create `root` and `purgery_root` directories |
| `gc` | Run garbage collection on expired incoming runs |
| `process-once` | Validate config, run GC, then process one batch of ready runs |

## Entry modes

Each manifest entry is classified as one of:

- **passthrough**: transferred directly to final storage by client rsync. Not staged-verified. Import authority is successful passthrough rsync.
- **postprocess**: transferred to purgatory/staging area, verified by server, processed via subprocesses, committed to final storage after processing succeeds.
- **covered**: descendant of a postprocessed directory. Not transferred independently. Skipped status.

## Import modes

### Passthrough entries

Passthrough entries are transferred directly from the client source tree to final server storage by a bulk rsync call. They do not enter the incoming staging area. A successful passthrough rsync is the import authority. Passthrough cleanup happens immediately after successful rsync, without waiting for server status.

The client writes a passthrough receipt (`passthrough.toml`) after each successful passthrough rsync call. The server uses this receipt to include passthrough entries in the run status.

### Postprocess entries

Postprocess entries are transferred by a separate bulk rsync call into the run's purgatory/staging area (`incoming/files/<sync.to>/`). The server prepares work-area input roots, runs subprocesses there, then commits selected output entry roots to final storage.

Both modes use the same final overlay rules after commit.

### Covered entries

If a directory entry matches a postprocess rule, descendant manifest entries under that directory are covered. Covered entries produce skipped status with `"covered by postprocessed ancestor directory"`.

## `prepare-run` response

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ..."
final_root = "/universe/synced/laptop"
purgatory_root = "/tmp/purgery/laptop/incoming/01ARZ.../files"
```

The client appends `/<sync.to>/` to `final_root` for the passthrough rsync destination, and `/<sync.to>/` to `purgatory_root` for the purgatory rsync destination.

## Manifest entry classification

```toml
[[entries]]
sync_name = "videos"
relative_path = "a.mp4"
kind = "regular_file"
mode = "postprocess"
postprocess_steps = ["compress-video"]

[[entries]]
sync_name = "videos"
relative_path = "notes.txt"
kind = "regular_file"
mode = "passthrough"

[[entries]]
sync_name = "videos"
relative_path = "album/cover.jpg"
kind = "regular_file"
mode = "covered"
covered_by = "videos/album"
```

## Run phases

```
incoming -> ready -> processing -> done
                                 failed
```

## Run states

| State | Meaning |
|-------|---------|
| `done` | All entries imported or skipped successfully |
| `partial` | Some entries imported, some failed or skipped |
| `failed` | No entries imported |
