# Purgery Protocol

## Lifecycle

### Path A: Pure passthrough (no postprocess roots)

```
client: local checks (config, executables)
client: resolve destinations over SSH -> server computes final storage paths (side-effect-free)
client: for each sync group:
           run passthrough rsync directly to final storage
           if delete_after_import=true:
             write durable cleanup state atomically
             delete confirmed passthrough regular files
```

### Path B: Postprocess run (one or more postprocess roots)

```
client: local checks (config, executables)
client: generate run ID, build manifest with entry classification
client: begin-run over SSH -> server creates incoming directory, returns paths
client: validate begin-run response envelope
client: write run.toml + manifest.toml to incoming dir (manifest contains only postprocess roots and covered descendants)
client: prepare-run over SSH -> server validates plan, returns transfer destinations
client: for each sync group:
           run passthrough rsync to final storage (non-postprocess entries)
           if delete_after_import=true:
             write durable cleanup state atomically
             delete confirmed passthrough regular files
           run purgatory rsync to incoming/files (postprocess entries)
client: finish-run over SSH -> server moves incoming -> ready
server: claim run by renaming ready -> processing
server: process postprocess entries (verify staged content, prepare work area, run subprocesses, commit outputs)
server: publish status for postprocess entries (postprocess via processing, covered as skipped)
server: write status.toml, move to done or failed
client: poll status, verify envelope, cleanup postprocessed regular files as soon as imported
```

## Server subcommands

| Subcommand | Purpose |
|------------|---------|
| `resolve-destinations --nickname <n>` | Side-effect-free destination resolution for pure passthrough groups |
| `begin-run --nickname <n> --run-id <id>` | Create incoming directory, write lease file, print TOML response with server paths |
| `prepare-run --nickname <n> --run-id <id>` | Validate run config and manifest (postprocess/covered entries only), return transfer destinations |
| `finish-run --nickname <n> --run-id <id>` | Move run from `incoming` to `ready` |
| `status --nickname <n> --run-id <id>` | Return `status.toml` from `done` or `failed` |
| `heartbeat-run --nickname <n> --run-id <id>` | Update lease file for an incoming run |
| `check` | Validate config and dependencies (side-effect-free) |
| `bootstrap` | Create `root` and `purgery_root` directories |
| `gc` | Run garbage collection on expired incoming runs |
| `process-once` | Validate config, run GC, then process one batch of ready runs |

## Entry modes

Each manifest entry in a server run is classified as one of:

- **postprocess**: transferred to purgatory/staging area, verified by server, processed via subprocesses, committed to final storage after processing succeeds.
- **covered**: descendant of a postprocessed directory. Not transferred independently. Skipped status.

Ordinary passthrough entries are not part of the server manifest. They are handled by direct client rsync without server bookkeeping.

## Import modes

### Passthrough entries (no server bookkeeping)

Passthrough entries are transferred directly from the client source tree to final server storage by a bulk rsync call. They do not enter the incoming staging area. A successful passthrough rsync is the import authority.

For sync groups with `delete_after_import = false`, no further bookkeeping is needed. The local file remains.

For sync groups with `delete_after_import = true`, the client writes a durable cleanup state file atomically after successful rsync. This state records the file identity (size, mtime, optional SHA-256) and authorizes deletion on restart. The client verifies local identity before deleting.

Passthrough entries do not appear in:
- The uploaded server manifest
- Server status files
- Server receipts

### Postprocess entries

Postprocess entries are transferred by a separate bulk rsync call into the run's purgatory/staging area (`incoming/files/<sync.to>/`). The server prepares work-area input roots, runs subprocesses there, then commits selected output entry roots to final storage.

### Covered entries

If a directory entry matches a postprocess rule, descendant manifest entries under that directory are covered. Covered entries produce skipped status with `"covered by postprocessed ancestor directory"`.

## Transfer roots

The client generates transfer roots from the classified manifest. Each transfer root is either:

- **Exact path root**: a regular file, symlink, or empty directory transferred as one independent entry. The rsync filter includes the path exactly.
- **Subtree path root**: a postprocessed directory whose entire subtree is transferred as a unit. The rsync filter includes the directory and all its descendants (`dir/**`).

Covered descendants are excluded from independent transfer roots. They are transferred only as part of the postprocessed directory subtree root.

### Transfer planning vs identity bookkeeping

Path planning produces `TransferPlanEntry` records (sync name, relative path, kind, classification, mode). These are lightweight and carry no identity-bearing fields (no size, mtime, or SHA-256).

Identity bookkeeping produces `ManifestEntry` records (with size, mtime, SHA-256) and `CleanupEntry` records (for durable local cleanup state). These are created only for entries that need them.

Identity bookkeeping is separated from path planning:

- Path planning (`TransferPlanEntry`) runs for all entries regardless of mode
- Identity is computed only when needed (delete_after_import=true passthrough regular files, or postprocess entries)
- No-delete passthrough entries get no identity bookkeeping

### Durable cleanup state

Cleanup state is stored at `$XDG_STATE_HOME/purgery/` or `~/.local/state/purgery/`. It is never stored in the temporary filter directory. The state file is written atomically and updated atomically after each deletion.

### Scoped postprocess rules

Postprocess rules may include a `for` field listing the sync group names the rule applies to:

```toml
[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["videos"]
```

- Omitted `for` means the rule applies to every sync group.
- `for = ["videos"]` applies only to the `videos` sync group.
- Empty `for` is invalid.
- Unknown sync names in `for` are rejected at config parse time.

When classifying a manifest entry, only rules applicable to its sync group participate. A rule is applicable when its `for` is omitted or the entry's sync group name appears in the list.

### Postprocessing requires delete_after_import

If a sync group has one or more applicable postprocess rules, `delete_after_import` must be `true`. This is validated statically at config parse time, before any filesystem walking or server operations. A sync group with applicable rules but `delete_after_import = false` is rejected.

### Sync group classes

Every sync group is one of two classes:

- **Passthrough group**: no applicable postprocess rules. `delete_after_import` may be true or false. No server-side bookkeeping.
- **Purgatory group**: one or more applicable postprocess rules. `delete_after_import` is guaranteed `true`. Participates in walk, manifest, upload, and server processing.

Passthrough groups are not included in the uploaded `run.toml`, server manifest, or status. In mixed invocations, passthrough destinations are resolved separately via `resolve-destinations`.

### No-rule groups

A sync group with no applicable postprocess rules (passthrough group) and `delete_after_import = false` is handled by one direct unfiltered rsync. No walking, scanning, classification, or bookkeeping occurs.

A sync group with no applicable postprocess rules but `delete_after_import = true` is handled by direct rsync plus durable local cleanup state. Scanning and identity computation are performed only for cleanup.

### Empty transfer sets

If a sync group has no passthrough transfer roots, the passthrough rsync is skipped. If a sync group has no purgatory transfer roots, the purgatory rsync is skipped.

## Server manifest contents

The uploaded server manifest (`manifest.toml`) contains only entries that require server-side bookkeeping:

- Postprocess roots (mode = `postprocess`)
- Covered descendants of postprocessed directory roots (mode = `covered`, with `covered_by`)

Ordinary passthrough entries are excluded.

### `prepare-run` validation

`prepare-run` validates the classification contract for every manifest entry:

- **mode**: must match the pattern classification (postprocess or covered)
- **covered_by**: for covered entries, must equal the source-relative path of the nearest postprocessed directory ancestor
- **postprocess_steps**: for covered entries, must be empty

If any covered entry has the wrong `covered_by` or non-empty `postprocess_steps`, the run is rejected.

## Destination resolution

### `resolve-destinations` response

For pure passthrough groups (no postprocess roots), the client calls `resolve-destinations` instead of `begin-run`/`prepare-run`:

```toml
protocol_version = 1
nickname = "laptop"

[[destinations]]
sync_name = "videos"
passthrough_dest = "/universe/synced/laptop/videos"

[[destinations]]
sync_name = "pictures"
passthrough_dest = "/universe/synced/laptop/pictures"
```

This command is side-effect-free. It does not create run directories, leases, manifests, or status files.

### `prepare-run` response

For postprocess runs, the client calls `prepare-run` after `begin-run`:

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

## Manifest entry classification (server manifest)

```toml
[[entries]]
sync_name = "videos"
relative_path = "a.mp4"
kind = "regular_file"
mode = "postprocess"
postprocess_steps = ["compress-video"]

[[entries]]
sync_name = "videos"
relative_path = "album/cover.jpg"
kind = "regular_file"
mode = "covered"
covered_by = "album"
```

Ordinary passthrough entries (e.g., `notes.txt` with mode `passthrough`) are not in this manifest.

## Server status

The server status file contains only postprocess and covered entries. Passthrough entries are absent.

Example:

```toml
run_id = "01ARZ..."
nickname = "laptop"
state = "done"

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_paths = ["laptop/videos/a.mp4"]
postprocess = ["compress-video"]

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/album/cover.jpg"
relative_path = "album/cover.jpg"
status = "skipped"
error = "covered by postprocessed ancestor directory"
```

## Run phases

```
incoming -> ready -> processing -> done
                                 failed
```

## Run states

| State | Meaning |
|-------|---------|
| `done` | All postprocess entries imported or skipped successfully |
| `partial` | Some postprocess entries imported, some failed or skipped |
| `failed` | No postprocess entries imported |
