# Purgery Protocol

## Lifecycle

### Path A: Pure passthrough (no --postprocess)

```
client: validate args
client: if --delete-after-import, capture durable local cleanup identity
client: source entry is transferred via rsync with trailing slashes stripped
client: direct rsync to USER@HOST:DESTINATION
client: if --delete-after-import, mark the transfer successful and remove only unchanged originals
(no server run, manifest, status polling, or finish-run)
```

### Path B: Postprocess (with --postprocess)

Server runs are postprocess-only. Every manifest entry must have non-empty `postprocess_steps`.

```
client: validate args (--postprocess requires --delete-after-import)
client: generate run ID
client: begin-run over SSH -> server creates incoming directory, returns paths
client: write run.toml + manifest.toml to server incoming dir
client: prepare-run over SSH -> server validates the destination, envelope, and requested steps
client: rsync source entry to server staging area (files/<source-name>)
client: persist local run state as upload_complete_finish_pending
client: finish-run over SSH -> server moves incoming -> ready
client: persist local state as waiting_for_terminal_state
client: wait using run-state; retry only on ready/processing
client: on terminal: read status
client: remove confirmed local original (server-confirmed cleanup)
client: persist local state as cleanup_complete
```

### Path C: Split (with --split)

Split has two paths: pure passthrough (one rsync filter transfer) and cleanup/postprocess (explicit discovery with serialized operations). They use different mechanisms and have different guarantees.

#### Pure passthrough split

With `--split` and neither `--delete-after-import` nor `--postprocess`:

```
client: validate args
client: if pattern is ".", run ordinary source-entry rsync
client: otherwise build constant filter rules and run one rsync
  (no Purgery-side discovery, no server run, no manifest, no cleanup state)
```

Pure passthrough split uses rsync filter semantics for the transfer. There is no Purgery-side candidate discovery or ancestor pruning. The contract is final destination effect under the generated filter rules.

For `--split "."` (source entry itself matched), ordinary source-entry rsync is used with no filters and no source trailing slash.

For all other patterns, one rsync invocation is constructed with these constant filter rules (actual argv values):

```
--include=*/
--include=<P-as-directory-payload>
--include=<P-as-entry>
--exclude=*
```

When shown as shell examples, filter arguments are quoted so the shell does not expand wildcards. The actual Rust `Command::args` argv values must not contain those quote characters.

```
rsync -a -m \
  --include='*/' \
  --include='<P-as-directory-payload>' \
  --include='<P-as-entry>' \
  --exclude='*' \
  -- <SOURCE>/ <HOST>:<TARGET>/
```

The source operand has a trailing slash so selected entries land under `<TARGET>`.

- `<P-as-entry>` is the pattern unchanged.
- `<P-as-directory-payload>` is the pattern with a trailing `/***` appended (after stripping any existing trailing `/`).

`--include=*/` keeps parent directories traversable. `--include=<P-as-directory-payload>` ensures matched directories transfer their full payload. `--include=<P-as-entry>` selects matching files, symlinks, and directory entries. `--exclude=*` prevents unrelated entries from being copied. `-m` / `--prune-empty-dirs` removes traversal-only directory scaffolding.

For directory sources, rsync always runs for non-dot patterns; when nothing matches the filter, rsync transfers nothing. For non-directory sources (regular files, symlinks), only `--split "."` can match; other patterns are no-op and exit successfully without invoking rsync.

Examples (actual argv):

```
--split "*.mp4"    → include=*/ include=*.mp4/*** include=*.mp4 exclude=*
--split "**/*.mp4" → include=*/ include=**/*.mp4/*** include=**/*.mp4 exclude=*
--split "Photos/"  → include=*/ include=Photos/*** include=Photos/ exclude=*
```

Pure passthrough split uses `--prune-empty-dirs` and prunes traversal-only empty directories. Empty directories selected only by the filter may not be created at the destination. Cleanup and postprocess split do not use this filter optimization.

#### Cleanup/postprocess split

With `--split` and either `--delete-after-import` or `--postprocess`:

```
client: validate args
client: discover split candidates under SOURCE using Purgery matcher
client: ancestor-prune matched roots
client: sort deterministically
if no match:
  log info, exit 0
for each matched root:
  run serialized non-split sync with root as source and target suffix
  wait for completion before next root
```

Cleanup/postprocess split uses Purgery's own pattern matcher for candidate discovery. Matched roots are ancestor-pruned (descendants of matched ancestors are not scheduled as separate operations, but their data remains part of the ancestor directory payload). The roots are sorted deterministically and processed serially — each operation completes entirely (transfer, status, cleanup, state resolution) before the next begins.

Source trailing slashes, `.`, and `..` are normalized before split discovery. `<SOURCE>` itself is matched as the relative sentinel `"."`.

## Server subcommands

| Command | Side effects | Returns |
|---------|-------------|---------|
| `begin-run --nickname N --run-id R` | Creates `incoming/R/` with lease, files/ dir | `BeginRunResponse` TOML |
| `prepare-run --nickname N --run-id R` | Validates destination, manifest envelope, and requested steps | `PrepareRunResponse` TOML |
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
destination = "/archive"
delete_after_import = true
```

The `destination` field is the target parent directory. It may be absolute or relative. For postprocess runs, a relative destination is resolved against the server's working directory during `prepare-run` and the `run.toml` is atomically rewritten with the absolute path. The server computes the final path as `{destination}/{source_entry_name}`; `work_dir` is never prepended.

## Manifest (manifest.toml)

A server-run manifest describes exactly one logical source entry and is uploaded only for postprocess runs. Direct passthrough never uploads a manifest.

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[entries]]
local_path = "/home/user/video.mp4"
staged_path = "files/video.mp4"
relative_path = "video.mp4"
kind = "regular_file"
size = 1048576
mtime_ns = 1700000000000000000
sha256 = "abc123..."
postprocess_steps = ["compress-video"]
```

For a directory source:

```toml
[[entries]]
local_path = "/home/user/Videos"
staged_path = "files/Videos"
relative_path = "Videos"
kind = "directory"
postprocess_steps = ["compress-video"]
```

- `staged_path` uses the format `files/<source-name>`.
- Non-empty `postprocess_steps` is required.
- The manifest contains exactly one entry for a non-split run.

## Status (status.toml)

```toml
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
local_path = "/home/user/video.mp4"
relative_path = "video.mp4"
status = "imported"
final_paths = ["/archive/video.mp4"]
postprocess = ["compress-video"]
```

### Final path computation

The source entry base final path is `<destination>/<source_entry_name>`.

- If `keep_original = true`, the original work entry commits to that base final path.
- Each expected postprocess output commits under the same parent as the base final path, using the output file name.
- For split nested entries, the split target suffix already points at the selected entry's relative parent, so the same rule applies.

Examples:

```
sync --postprocess compress -- ./video.mp4 host:/archive
  original, if kept: /archive/video.mp4
  output video.Z.webm: /archive/video.Z.webm

sync --postprocess compress -- ./Videos/2024/a.mp4 host:/archive/2024
  original, if kept: /archive/2024/a.mp4
  output a.Z.webm: /archive/2024/a.Z.webm
```

The nickname is operational metadata and does not appear in final_paths.

## RunStateResponse

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
phase = "processing"
terminal = false
message = "processing entry 1/1"
updated_at_unix_secs = 1234567890
observed_at_unix_secs = 1234567891
```

Phases: `incoming`, `ready`, `processing`, `done`, `failed`, `corrupt`, `not_found`.
- Terminal (`terminal = true`): `done`, `failed`.
- Non-terminal (`terminal = false`): `incoming`, `ready`, `processing`, `corrupt`, `not_found`.
`not_found` means the server does not know about the run; it is not a terminal success and the client treats it as an error.

## Client run state persistence

The client persists per-run state under `{state_dir}/runs/{nickname}-{run_id}/state.toml`. This enables crash-safe resume of waiting and cleanup.

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
host = "user@server"
server_command = "purgery-server"
manifest = "..."
run_config = "..."
phase = "waiting_for_terminal_state"
```

Fields:
- `host` — the SSH host from the original destination.
- `server_command` — the remote server command.
- `terminal_status` — optional serialized `RunStatus` TOML, set when the phase becomes `terminal_status_seen`. Enables recovery without re-reading from the server.

Phases: `upload_complete_finish_pending`, `waiting_for_terminal_state`, `terminal_status_seen`, `cleanup_complete`, `abandoned`, `corrupt`.
