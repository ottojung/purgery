# Purgery Protocol

## Lifecycle

### Path A: Pure passthrough (no --transform)

```
client: validate args
client: if --delete-after-import, capture durable local cleanup identity
client: source entry is transferred via rsync with trailing slashes stripped
client: direct rsync to USER@HOST:DESTINATION
client: if --delete-after-import, mark the transfer successful and remove only unchanged originals
(no server run, manifest, status polling, or finish-run)
```

### Path B: Transform (with --transform)

Server runs are transform-only. Every manifest entry must have a transform.

```
client: validate args (--transform requires --delete-after-import)
client: generate run ID
client: begin-run over SSH -> server creates incoming directory, returns paths
client: write run.toml + manifest.toml to server incoming dir
client: prepare-run over SSH -> server validates the destination, envelope, and requested transform
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

Split has two paths: pure passthrough (one rsync filter transfer) and cleanup/transform (explicit discovery with serialized operations). They use different mechanisms and have different guarantees.

#### Pure passthrough split

With `--split` and neither `--delete-after-import` nor `--transform`:

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
--include=<P-as-nested-directory-payload>   (patterns without "/" only)
--include=<P-as-entry>
--exclude=*
```

When shown as shell examples, filter arguments are quoted so the shell does not expand wildcards. The actual process argv values must not contain those quote characters.

```
rsync -a -m \
  --include='*/' \
  --include='<P-as-directory-payload>' \
  --include='<P-as-nested-directory-payload>' \
  --include='<P-as-entry>' \
  --exclude='*' \
  -- <SOURCE>/ <HOST>:<TARGET>/
```

The source operand has a trailing slash so selected entries land under `<TARGET>`.

- `<P-as-entry>` is the pattern unchanged.
- `<P-as-directory-payload>` is the pattern with a trailing `/***` appended (after stripping any existing trailing `/`).
- `<P-as-nested-directory-payload>` is the same but prefixed with `**/` so directories whose names match a component-only pattern at any nesting depth transfer their full payload. Only emitted for patterns without `/`.

`--include=*/` keeps parent directories traversable. `--include=<P-as-directory-payload>` ensures top-level matched directories transfer their full payload. `--include=<P-as-nested-directory-payload>` ensures nested directories whose names match a component-only pattern transfer their full payload. `--include=<P-as-entry>` selects matching files, symlinks, and directory entries. `--exclude=*` prevents unrelated entries from being copied. `-m` / `--prune-empty-dirs` removes traversal-only directory scaffolding.

For directory sources, rsync always runs for non-dot patterns; when nothing matches the filter, rsync transfers nothing. For non-directory sources (regular files, symlinks), only `--split "."` can match; other patterns are no-op and exit successfully without invoking rsync.

Examples (actual argv):

```
--split "*.mp4"    → include=*/ include=*.mp4/*** include=**/*.mp4/*** include=*.mp4 exclude=*
--split "**/*.mp4" → include=*/ include=**/*.mp4/*** include=**/*.mp4 exclude=*
--split "Photos/"  → include=*/ include=Photos/*** include=Photos/ exclude=*
```

Pure passthrough split uses `--prune-empty-dirs` and prunes traversal-only empty directories. Empty directories selected only by the filter may not be created at the destination. Cleanup and transform split do not use this filter optimization.

#### Cleanup/transform split

With `--split` and either `--delete-after-import` or `--transform`:

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

Cleanup/transform split uses Purgery's own pattern matcher for candidate discovery. Matched roots are ancestor-pruned (descendants of matched ancestors are not scheduled as separate operations, but their data remains part of the ancestor directory payload). The roots are sorted deterministically and processed serially — each operation completes entirely (transfer, status, cleanup, state resolution) before the next begins.

Source trailing slashes, `.`, and `..` are normalized before split discovery. `<SOURCE>` itself is matched as the relative sentinel `"."`.

## Server subcommands

| Command | Side effects | Returns |
|---------|-------------|---------|
| `begin-run --nickname N --run-id R` | Creates `incoming/R/` with lease, files/ dir | `BeginRunResponse` TOML |
| `prepare-run --nickname N --run-id R` | Validates destination, manifest envelope, and requested transform | `PrepareRunResponse` TOML |
| `finish-run --nickname N --run-id R` | Moves run from incoming to ready | (none) |
| `heartbeat-run --nickname N --run-id R` | Extends incoming lease | (none) |
| `run-state --nickname N --run-id R` | None | `RunStateResponse` TOML |
| `status --nickname N --run-id R` | None | `RunStatus` TOML |
| `process-run --nickname N --run-id R` | Start global GC and wait for it before returning; drive only the target run by claiming/processing/recovering it; if target processing is locked by another processor, no-op; does not process unrelated ready/processing runs | (none) |
| `process-once` | Run global GC, recover unlocked processing runs (respecting active processor locks), process ready runs | (none) |
| `check` | None | (none) |
| `gc` | Collects expired runs | (none) |

## Run phases

```
incoming → ready → processing → done
                            ↘ failed
```

A run moves from incoming to ready when the client calls `finish-run`. The server moves ready runs to processing via targeted `process-run` (triggered by the client) or batch `process-once` (operator/daemon). On completion, runs move to done or failed.

A processing run may be actively mutated only by a process holding the run's `processor.lock`. If `process-run` or `process-once` observes a processing run and the lock is busy, it treats that run as actively owned and does not recover or replay it. If the lock is free, the run is considered abandoned and may be recovered.

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

The `destination` field is the target parent directory. It may be absolute or relative. For transform runs, a relative destination is resolved against the server's working directory during `prepare-run` and the `run.toml` is atomically rewritten with the absolute path.

The source entry base final path is `<destination>/<source_entry_name>`. `{target_directory}` is `<destination>` (the parent of the base final path).

Transform programs are responsible for placing outputs at the resolved expected output paths. Purgery does not move or commit transform outputs. After the transform exits successfully, Purgery checks that each declared expected output exists. `expected_outputs` are path patterns: relative patterns resolve against `<DESTINATION>`, absolute patterns are used as-is, and `{target_directory}` is allowed.

For non-transform entries, the server commits the work entry directly to the base final path.

`work_dir` is never final storage.

## Manifest (manifest.toml)

A server-run manifest describes exactly one logical source entry and is uploaded only for transform runs. Direct passthrough never uploads a manifest.

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
transform = "compress-video"
```

For a directory source:

```toml
[[entries]]
local_path = "/home/user/Videos"
staged_path = "files/Videos"
relative_path = "Videos"
kind = "directory"
transform = "compress-video"
```

- `staged_path` uses the format `files/<source-name>`.
- A non-empty `transform` field is required.
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
final_paths = ["/archive/video.Z.webm"]
transform = "compress-video"
```

### Final path computation

The source entry base final path is `<destination>/<source_entry_name>`. `{target_directory}` is `<destination>`.

For non-transform entries, the server commits the work entry to the base final path.

For transform entries, `final_paths` in the status records the resolved expected output paths whose existence Purgery confirmed after the transform. Relative `expected_outputs` resolve against `<DESTINATION>`, absolute paths are used as-is, and `{target_directory}` is allowed. If `expected_outputs = []`, no paths are checked and `final_paths` is empty. Purgery does not move or commit transform outputs; it only checks that declared expected outputs exist. Transformed inputs are consumed by the transform flow and are never committed as final outputs.

Examples:

```
sync --transform compress -- ./video.mp4 host:/archive
  expected_outputs = ["{target_directory}/{file_stem}.Z.webm"]
  final_paths = ["/archive/video.Z.webm"]

sync --transform compress -- ./Videos/2024/a.mp4 host:/archive/2024
  expected_outputs = ["{target_directory}/{file_stem}.Z.webm"]
  final_paths = ["/archive/2024/a.Z.webm"]
```

The short relative form `expected_outputs = ["{file_stem}.Z.webm"]` is also valid and resolves against `<DESTINATION>`.

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

## Version compatibility

Purgery uses two independent version concepts.

### `protocol_version`

The protocol version describes the shape or family of machine-readable protocol messages and durable file envelopes. It must match exactly for client–server communication and for durable-file deserialization. Increment it when the wire format or durable-file structure changes.

Current value: `1`.

### `purgery_version`

The `purgery_version` field records which Purgery application version produced a protocol message or durable file. It is a semver string (`MAJOR.MINOR.PATCH`).

Two versions are compatible when their major **and** minor components are equal. Patch versions may differ.

Examples:

```
0.1.0 client with 0.1.7 server: compatible
0.1.7 client with 0.1.0 server: compatible
0.1.x with 0.2.x: incompatible
0.1.x with 1.0.x: incompatible
0.1.x with 0.1.x: compatible
```

### Client–server compatibility check

Before starting or resuming a server-backed operation, the client calls the server `version` command. The response is TOML:

```toml
protocol_version = 1
purgery_version = "0.1.0"
```

The client validates both fields:

- `protocol_version` must equal the client’s `PROTOCOL_VERSION`.
- `purgery_version` must be major/minor-compatible with the client’s package version.

If either check fails, the client refuses the operation. Direct passthrough rsync (without `purgery-server`) does not perform this check.

### Durable file version policy

Every durable Purgery-owned TOML file carries `purgery_version`. The current domain structs require the field — old files that lack it do not deserialize as current types.

Durable files include:

- `run.toml`, `manifest.toml` — client-written input for a server run.
- `lease.toml` — incoming run lease.
- `status.toml` — terminal or failure status.
- `progress.toml` — processing-phase progress.
- `state.toml` — client-side persisted run state.
- `cleanup-*.toml` — durable cleanup state.
- Protocol response TOML from server subcommands.

Atomic temporary files (e.g. `*.toml.tmp`) are implementation details and are not subject to version policy.

### Incompatible file policy

When Purgery encounters a standalone durable file whose `purgery_version` is missing, malformed, or major/minor-incompatible, it must:

- warn with the file path and version context;
- leave the file exactly where it is;
- **not** rename or delete the file;
- **not** overwrite it with current-version state;
- **not** move its containing run to `failed`;
- **not** write a replacement status;
- continue as if that file or run does not exist.

This is a safety rule. Purgery must not reinterpret old state as current state, and must not automatically migrate or destroy files it cannot safely interpret. Operator intervention or an explicit future migration tool can handle old state.

### Malformed current-version files

A file with a compatible `purgery_version` but malformed content, wrong envelope, or otherwise invalid current-version structure is **not** an incompatibility problem. Purgery may treat it according to the relevant command’s normal error handling (write a failure status, report corruption, etc.). Do not confuse malformed current-version files with old-version incompatibility.

### ClientRunState embedded-field exception

`ClientRunState` may embed serialized `manifest`, `run_config`, or `terminal_status`. These embedded values are not independently discovered files — they are part of the same durable state object, written by the same client version. The client treats incompatible embedded data as malformed current state, not as standalone incompatible files. This exception exists so that the scan of `state.toml` files does not need to recursively version-check each embedded blob.

### Rationale

- Avoid cross-version semantic reuse of producer metadata.
- Avoid deleting or rewriting state that the current binary cannot safely interpret.
- Allow patch-version interoperability so security fixes and minor improvements do not break compatibility.
- Leave old state in place for operator inspection and manual cleanup.
- Keep cleanup and deletion authority conservative by refusing to act on incompatible cleanup state.


