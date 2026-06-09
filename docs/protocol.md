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

## Transfer roots

The client generates transfer roots from the classified manifest. Each transfer root is either:

- **Exact path root**: a regular file, symlink, or empty directory transferred as one independent entry. The rsync filter includes the path exactly.
- **Subtree path root**: a postprocessed directory whose entire subtree is transferred as a unit. The rsync filter includes the directory and all its descendants (`dir/**`).

Covered descendants are excluded from independent transfer roots. They are transferred only as part of the postprocessed directory subtree root.

### Empty transfer sets

If a sync group has no passthrough transfer roots, the client skips the passthrough rsync and does not write a successful passthrough receipt for that sync. If a sync group has no purgatory transfer roots, the client skips the purgatory rsync.

### `prepare-run` validation

`prepare-run` validates the full classification contract for every manifest entry:

- **mode**: must match the pattern classification (postprocess, passthrough, or covered)
- **covered_by**: for covered entries, must equal the source-relative path of the nearest postprocessed directory ancestor
- **postprocess_steps**: for covered entries, must be empty

If any covered entry has the wrong `covered_by` or non-empty `postprocess_steps`, the run is rejected.

## `prepare-run` response

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ..."

[[destinations]]
sync_name = "videos"
passthrough_dest = "/universe/synced/laptop/videos"
purgatory_dest = "/tmp/purgery/laptop/incoming/01ARZ.../files/videos"

[[destinations]]
sync_name = "pictures"
passthrough_dest = "/universe/synced/laptop/pictures"
purgatory_dest = "/tmp/purgery/laptop/incoming/01ARZ.../files/pictures"
```

The client uses the per-sync destinations as rsync targets. For passthrough, it constructs `host:<passthrough_dest>/`. For purgatory, it constructs `host:<purgatory_dest>/`.

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
covered_by = "album"
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
