# Purgery Protocol

## Lifecycle

### Path A: Pure passthrough (no postprocess roots)

```
client: local checks (config, executables)
client: resolve destinations over SSH -> server computes final storage paths (side-effect-free)
client: for each passthrough group:
            run direct unfiltered rsync to final storage
            if PassthroughDeleteAfterImport:
              write durable cleanup state atomically
              remove confirmed local originals
```

There is no per-entry filtered transfer loop in the pure passthrough path.
Every passthrough group uses direct unfiltered rsync.
No passthrough group is transferred more than once.

### Path B: Mixed invocation (purgatory groups + passthrough groups)

```
client: local checks (config, executables)
client: partition sync groups into execution classes:
         PassthroughNoDelete:  no rules, delete_after_import=false
         PassthroughDeleteAfterImport:  no rules, delete_after_import=true
         Purgatory:  one or more rules, delete_after_import=true
client: generate run ID, build manifest with entry classification (purgatory groups only)
client: if passthrough-only groups exist:
         resolve destinations via resolve-destinations (side-effect-free)
client: begin-run over SSH -> server creates incoming directory, returns paths
client: validate begin-run response envelope
client: write purgatory-only run.toml + filtered manifest.toml to incoming dir
        run.toml contains only purgatory sync groups and rules applicable to them
client: prepare-run over SSH -> server validates plan, returns purgatory destinations
client: after prepare-run succeeds, perform all archive-affecting rsyncs:
         for each passthrough-only group:
           direct unfiltered rsync to final storage
           if PassthroughDeleteAfterImport:
             write durable cleanup state, remove confirmed local originals
         for each purgatory group:
           run passthrough rsync to final storage (non-postprocess entries)
           run purgatory rsync to incoming/files (postprocess entries)
client: persist local postprocess run state as upload_complete_finish_pending
client: finish-run over SSH -> server moves incoming -> ready
client: persist local state as waiting_for_terminal_state
client: wait using run-state; retry only on ready/processing/incoming
client: on corrupt -> write local corrupt tombstone, stop, do not delete
client: on not_found -> write local abandoned tombstone, stop, do not delete
client: on transport failure or malformed response -> stop with state preserved
client: after terminal run-state, read terminal status via status command
client: if status fails (transport/parse/envelope) -> write corrupt, stop, do not delete
client: cleanup only imported postprocess entries whose local identity still matches
client: mark cleanup complete and remove local run state
server: claim run by renaming ready -> processing
server: process postprocess entries (writes progress.toml during processing)
server: before publishing terminal status, best-effort write state=publishing_status
server: write status.toml, move to done or failed
```

Passthrough groups are handled entirely outside the purgatory run lifecycle.
The purgatory transfer loop operates only on purgatory groups — it never looks up passthrough groups in the prepare-run destination map.

No final-archive rsync happens before `prepare-run` succeeds. The side-effect-free `resolve-destinations` call may happen earlier, but actual archive-affecting transfers are deferred until after the purgatory run passes server validation.

Postprocess entries are transferred to a staging area (`incoming/files/<sync.to>/`), not to final storage. The server processes them in the staging area using isolated work areas, and only commits outputs to final storage under the named archive root after processing succeeds. If processing fails, no output reaches the final archive.

## Server subcommands

| Subcommand | Purpose |
|------------|---------|
| `resolve-destinations --nickname <n>` | Side-effect-free destination resolution for pure passthrough groups |
| `begin-run --nickname <n> --run-id <id>` | Create incoming directory, write lease file, print TOML response with server paths |
| `prepare-run --nickname <n> --run-id <id>` | Validate purgatory run config and manifest (postprocess/covered entries only), return transfer destinations |
| `finish-run --nickname <n> --run-id <id>` | Move run from `incoming` to `ready` (idempotent: safe to call again if already past incoming) |
| `status --nickname <n> --run-id <id>` | Return `status.toml` from `done` or `failed` (terminal-only; fails if run is not yet terminal) |
| `run-state --nickname <n> --run-id <id>` | Report current filesystem phase without requiring terminal status. Returns `incoming`, `ready`, `processing`, `done`, `failed`, or `not_found`. When `processing`, may include progress details from `progress.toml` |
| `heartbeat-run --nickname <n> --run-id <id>` | Update lease file for an incoming run |
| `check` | Validate config and dependencies (side-effect-free) |
| `bootstrap` | Create all named root directories and `work_dir` |
| `gc` | Run garbage collection on expired incoming runs |
| `process-once` | Validate config, run GC, then process one batch of ready runs |

## Entry modes

Each manifest entry in a server run is classified as one of:

- **postprocess**: transferred to purgatory/staging area, verified by server, processed via subprocesses, committed to final storage under the named archive root after processing succeeds.
- **covered**: descendant of a postprocessed directory. Not transferred independently. Skipped status.

Ordinary passthrough entries are not part of the server manifest. They are handled by direct client rsync without server bookkeeping.

## Import modes

### Passthrough entries (no server bookkeeping)

Passthrough entries are transferred directly from the client source tree to final server storage by a bulk rsync call. They do not enter the incoming staging area. A successful passthrough rsync is the import authority.

For sync groups with `delete_after_import = false`, no further bookkeeping is needed. The local entry remains.

For sync groups with `delete_after_import = true`, the client writes a durable cleanup state file atomically before rsync with `rsync_succeeded = false`, and updates the success marker atomically after rsync succeeds. This state records the entry identity per kind (size, mtime, and SHA-256 for regular files; link target for symlinks; subtree entries for directories) and authorizes cleanup on restart. Regular files without SHA identity are not deletion-authorizing. The client verifies local identity before removing.

Passthrough entries do not appear in:
- The uploaded server manifest
- Server status files
- Server receipts

### Postprocess entries

Postprocess entries are transferred by a separate bulk rsync call into the run's purgatory/staging area (`incoming/files/<sync.to>/`). The server prepares work-area input roots, runs subprocesses there, then commits selected output entry roots to final storage under the named archive root.

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

Cleanup state is stored in the client's configured `state_dir` (`state_dir` in `client.toml`, a required absolute path). It is never stored in the temporary filter directory. The state file is written atomically and updated atomically after each deletion.

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

If a sync group has one or more applicable postprocess rules, `delete_after_import` must be `true`. This is an intentional conformance tradeoff (see [import semantics](docs/design/import-semantics.md#postprocessing-conformance-and-import-and-retire)). Because Purgery does not retain indefinite source-entry metadata, a postprocessed import is import-and-retire: the confirmed local original is removed after successful server-confirmed import.

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
- **scoped rules**: every `postprocess.rules[].for` entry must reference a sync name that exists in the run config. Empty `for` is rejected.

If any covered entry has the wrong `covered_by` or non-empty `postprocess_steps`, the run is rejected.

### Uploaded run config validation

The uploaded `run.toml` is validated through a central API in `purgery-core`:

- `RunConfig::from_toml` parses TOML and validates `postprocess.rules.for` scoping.
- `RunConfig::validate_uploaded_purgatory_run` confirms every sync has `delete_after_import = true` and `for` lists are valid.

The server applies these validations at every lifecycle point that reads the uploaded run config:

- `resolve-destinations` uses structural validation only (for lists). It does not require `delete_after_import = true`, since it is also called for pure passthrough groups.
- `prepare-run` performs full purgatory validation before any manifest processing, final-path validation, or storage mutation.
- `process_processing_run` and `recover_or_process_processing_run` perform full purgatory validation again before `RunPlan::build` and before final-storage mutation.

`RunPlan::build` defensively validates the run config independently, so that invalid uploaded run configs are rejected even if a caller forgets to validate beforehand.

## Destination resolution

The server maintains a set of named archive roots. Client sync `to` fields reference a root by name. The `to` field format is:

```text
<root-name>[/<path-under-root>]
```

Examples:
- `to = "univ/videos"` — root named "univ", inside the "videos" subdirectory
- `to = "system"` — root named "system", directly under the root

### `resolve-destinations` response

For pure passthrough groups (no postprocess roots), the client calls `resolve-destinations` instead of `begin-run`/`prepare-run`:

```toml
protocol_version = 1
nickname = "laptop"

[[destinations]]
sync_name = "videos"
passthrough_dest = "/universe/synced/videos"

[[destinations]]
sync_name = "pictures"
passthrough_dest = "/universe/synced/pictures"
```

This command is side-effect-free. It does not create run directories, leases, manifests, or status files. The resolved destinations are absolute paths under the named archive roots. The client nickname does not appear in resolved destinations.

### `prepare-run` response

For postprocess runs, the client calls `prepare-run` after `begin-run`:

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ..."

[[destinations]]
sync_name = "videos"
passthrough_dest = "/universe/synced/videos"
purgatory_dest = "/var/lib/purgery/work/laptop/incoming/01ARZ.../files/videos"

[[destinations]]
sync_name = "pictures"
passthrough_dest = "/universe/synced/pictures"
purgatory_dest = "/var/lib/purgery/work/laptop/incoming/01ARZ.../files/pictures"
```

The client uses the per-sync destinations as rsync targets. For passthrough, it constructs `host:<passthrough_dest>/`. For purgatory, it constructs `host:<purgatory_dest>/`.

### Work area isolation

Postprocess work areas are under:

```text
{work_dir}/{nickname}/processing/{run_id}/work/
```

This is inside the run's processing directory, so it is naturally cleaned or recovered with the processing run. On successful completion (`Done`), the work area is removed before the run moves to `done`. On failure (`Failed` or `Partial`), the work area stays with the run directory as it moves to `failed` or `done`.

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
final_paths = ["univ/videos/a.mp4"]
postprocess = ["compress-video"]

[[entries]]
kind = "regular_file"
sync_name = "videos"
local_path = "/home/user/Videos/album/cover.jpg"
relative_path = "album/cover.jpg"
status = "skipped"
error = "covered by postprocessed ancestor directory"
```

`final_paths` entries are root-qualified relative archive paths. The first component is the named root, followed by the path under that root and the relative entry path. The client nickname is not present in `final_paths`.

Status entries with a `final_paths` value of `univ/videos/a.mp4` mean the file is at `/universe/synced/videos/a.mp4` (under the root named "univ"), not at any path containing the nickname.

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

### Run phase reporting

The server provides two commands for reading run status:

* **`status`** — returns terminal status from `done/` or `failed/`. Fails if the run is not yet terminal.
* **`run-state --nickname <n> --run-id <id>`** — returns the current filesystem phase without requiring terminal status.

`run-state` returns TOML:

```toml
protocol_version = 1
nickname = "laptop"
run_id = "..."
phase = "processing" # incoming | ready | processing | done | failed | not_found | corrupt
terminal = false
message = "..."
updated_at_unix_secs = 123 # last server-side phase/progress update time, or 0 if unknown
observed_at_unix_secs = 456 # query wall-clock time on the server
```

Semantics of the fields:

* `phase`: the current filesystem phase (`incoming`, `ready`, `processing`, `done`, `failed`, `not_found`, `corrupt`).
* `terminal`: `true` only for `done` or `failed` when a valid, envelope-matching `status.toml` exists. All other phases have `terminal = false`.
* `corrupt`: a terminal phase directory exists but `status.toml` is missing, malformed, or envelope-mismatched. `corrupt` is never cleanup authority.
* `updated_at_unix_secs`: the last known server-side phase or progress update time. For `processing` with valid `progress.toml`, this comes from the progress file. For missing/malformed progress, this uses the directory modification time. For `not_found`, this is `0`. Never set this to query time if the true update time is unknown.
* `observed_at_unix_secs`: the wall-clock time when the server evaluated this response.
* `message`: human-readable phase description, including progress details when processing.
* `not_found` and `corrupt` are never cleanup authority.

### Processing progress

While a run is in `processing/`, the server writes a progress file:

```text
{work_dir}/{nickname}/processing/{run_id}/progress.toml
```

The file is updated atomically when processing starts, before and after each manifest entry, before and after each postprocess step, and periodically while a long-running subprocess is executing. The client can query `run-state` to see progress details and that the server is still working.

### Client-side durable postprocess state

After all purgatory uploads have completed but before calling `finish-run`, the client persists local run state:

```text
{state_dir}/runs/{nickname}-{run_id}/state.toml
```

The file contains the local phase, the full manifest, and the run config. Local phases:

* `upload_complete_finish_pending`: uploads complete, about to call `finish-run`.
* `waiting_for_terminal_state`: `finish-run` accepted, waiting for server to reach terminal phase.
* `terminal_status_seen`: terminal status has been read; cleanup may proceed.
* `cleanup_complete`: cleanup finished; local state may be removed.
* `abandoned`: run was lost or abandoned; no deletion authorised.
* `corrupt`: server state is corrupt; no deletion authorised.

### Client wait-loop phase handling

When the client calls `run-state` while waiting for a terminal state, it maps the returned phase as follows:

| `run-state` phase | Client action |
|---|---|
| `ready` | Wait (poll again). |
| `processing` | Wait (poll again). |
| `done` or `failed` with `terminal = true` | Proceed to read terminal status. |
| `not_found` | Write `ClientRunPhase::Abandoned` tombstone. Return error. No deletion. |
| `corrupt` | Write `ClientRunPhase::Corrupt` tombstone. Return error. No deletion. |
| `incoming` | Return error with state preserved. Not a normal wait phase after `finish-run`. |
| Any other phase | Return error with local state preserved. No deletion. |
| Transport/SSH/command failure | Return error with local state preserved. Do not retry forever. |
| Malformed response (unparseable TOML) | Return error with local state preserved. Do not retry forever. |

Only `ready` and `processing` are indefinite wait phases in the general wait loop. `incoming` is accepted only during `UploadCompleteFinishPending` resume — seeing it while in `WaitingForTerminalState` is a protocol inconsistency and the client stops. All other phases or errors terminate the current invocation.

### Terminal status handling

After `run-state` returns a terminal phase with `terminal = true`, the client calls `status`:

| `status` result | Client action |
|---|---|
| Success, parseable, envelope matches | Cleanup may proceed. |
| Transport/SSH/command failure | Return error with `TerminalStatusSeen` state preserved. No infinite retry. |
| Malformed (unparseable) | Write `ClientRunPhase::Corrupt` tombstone. No deletion. |
| Envelope mismatch | Write `ClientRunPhase::Corrupt` tombstone. No deletion. |

### Client resume behavior

`resume_pending_postprocess_runs` handles each persisted `ClientRunPhase`:

| Phase | Action |
|---|---|
| `CleanupComplete` | Remove local state silently. |
| `Abandoned` | Return error blocking new sync. Tombstone persists until explicit clearing. |
| `Corrupt` | Return error blocking new sync. Tombstone persists until explicit clearing. |
| `UploadCompleteFinishPending` | Query server: if `incoming`, call `finish-run` then wait; if `ready`/`processing`, proceed to waiting; if `done`/`failed` with terminal, read status/cleanup; if `not_found`, mark abandoned; if `corrupt`, mark corrupt; on transport/malformed, error with state preserved. |
| `WaitingForTerminalState` | Wait via `run-state`. |
| `TerminalStatusSeen` | Go directly to terminal status verification/cleanup. Do not call wait loop. Do not rewrite to `WaitingForTerminalState`. |

### Abandoned/corrupt tombstones block normal sync

If any `Abandoned` or `Corrupt` tombstone exists under `state_dir/runs/`, a normal `sync-and-cleanup` invocation returns an error before starting any new sync work. The user must manually clear the tombstone or use a future explicit command.

Tombstones are never auto-removed. They are durable diagnostics.

### Processing progress fields

Progress updates carry the following context:

```toml
protocol_version = 1
nickname = "laptop"
run_id = "..."
phase = "processing"
state = "step_running"       # processing_started | processing_entry | step_started | step_running | step_finished | publishing_status
entry_index = 3              # current entry index (0-based)
entry_total = 10             # total entries in manifest
current_entry = "videos/a.mp4"  # relative path of current entry
current_step = "compress-video" # current postprocess step name
started_at_unix_secs = ...
updated_at_unix_secs = ...
```

### Timestamp semantics

- `started_at_unix_secs`: the stable wall-clock time when the server began processing the current run. This value is set once on the first progress write and preserved across all subsequent updates for the same run. It is never reset. If an existing `progress.toml` is present and its envelope (`nickname`, `run_id`) matches the current run, `started_at_unix_secs` is read from it rather than recomputed. If the existing file is missing, malformed, or envelope-mismatched, a fresh `started_at_unix_secs` is initialized to the current time.
- `updated_at_unix_secs`: the wall-clock time of the current progress update. This value advances independently on every write. It is always set to `now` and never preserved from a prior update.
- The first progress write for a run initializes both `started_at` and `updated_at` to the current time. All subsequent writes preserve `started_at` and update only `updated_at`.

The `state` field transitions:

- `processing_started` — written before any entries are processed (run-level)
- `processing_entry` — written before each manifest entry (per-entry)
- `step_started` — before a postprocess step subprocess is spawned (per-entry)
- `step_running` — periodically while a long-running subprocess executes (per-entry)
- `step_finished` — after a postprocess step succeeds (per-entry)
- `publishing_status` — before terminal `status.toml` is published, best-effort (run-level)

### Run-level vs per-entry progress

| Type | `state` | `entry_index` | `entry_total` | `current_entry` | `current_step` |
|------|---------|---------------|---------------|-----------------|----------------|
| Run-level | `processing_started`, `publishing_status` | May be 0 | Coherent total (N) | Empty `""` | Empty `""` |
| Per-entry | `processing_entry` | Real position | Coherent total (N) | Real relative path | Empty `""` |
| Per-entry | `step_started`, `step_running`, `step_finished` | Real position | Coherent total (N) | Real relative path | Real step name |

Run-level progress has no current entry. Empty `current_entry` and `current_step` are not sentinel values — they mean the progress is about the run as a whole, not a specific entry.

For per-entry progress:

- `entry_total > 0`
- `current_entry != ""`
- `entry_index < entry_total`
- `entry_index` is zero-based: the first real manifest entry has `entry_index = 0`.

`entry_total` is never `0` for per-entry progress. `entry_index = 0` is valid (first entry).

`ProgressUpdate` is always fully populated. No field is left as `0` to mean "the caller should fill this in later."

### Progress write validation

Progress producers validate progress state semantics before publishing progress. Invalid progress is not published. Progress publication failures are warning-level and do not affect import correctness.

Validated invariants:

- **Run-level states** (`processing_started`, `publishing_status`): `current_entry` and `current_step` must be empty.
- **Per-entry state** (`processing_entry`): `entry_total > 0`, `entry_index < entry_total`, `current_entry` non-empty, `current_step` empty.
- **Per-entry step states** (`step_started`, `step_running`, `step_finished`): `entry_total > 0`, `entry_index < entry_total`, `current_entry` non-empty, `current_step` non-empty.
- Unknown states are rejected.

### Progress file retention

`progress.toml` is written inside the run's `processing/` directory. When the run is finalized, the `processing/` directory is renamed to `done/` or `failed/` as a whole, so the progress file may be visible in the terminal directory. This retention is diagnostic only and is not a protocol guarantee. Clients must not use retained progress as cleanup authority.

### Progress write failure

If a progress file write fails (I/O error after validation), the server logs a warning with structured context and continues processing. A progress write failure must not fail an otherwise successful import. Progress is observational only and never authorizes cleanup.

All progress writes use a best-effort helper that logs a warning on failure. The warning includes all progress fields: `nickname`, `run_id`, `state`, `entry_index`, `entry_total`, `current_entry`, `current_step`, and the error. Silent failures (`let _ = write_progress(...)`) are replaced with explicit warning-level logging.

An invalid progress update must not overwrite an existing valid `progress.toml`. Validation runs before writing, so an invalid update is rejected before any file mutation occurs, leaving the last valid progress intact.

### Tombstone persistence failure messages

If writing an `Abandoned` or `Corrupt` tombstone fails, the returned error includes both:

- the original condition that triggered the tombstone (`not_found`, server `corrupt`, malformed terminal status, envelope mismatch);
- the fact that the tombstone could not be persisted.

Even if the tombstone write fails, the client still does not delete anything. The error message directs the user to the state directory for manual resolution.

### Safety-state persistence

Client-persisted run state (`state_dir/runs/{nickname}-{run_id}/state.toml`) follows different rules from progress:

- **`WaitingForTerminalState`**: Must be persisted before entering the wait loop. Failure to persist stops the invocation before any waiting or deletion.
- **`TerminalStatusSeen`**: Must be persisted before calling the server status command. Failure to persist stops the invocation before any deletion.
- **`CleanupComplete`**: Must be persisted after deletion. Failure to persist leaves the old state in place so recovery can distinguish complete from interrupted cleanup.
- **`Abandoned`** and **`Corrupt`** (tombstones): Must be persisted before returning the error. Failure to persist returns an error that clearly says the tombstone could not be written. In all cases the client stops without deletion.

Safety-state writes are not best-effort. If a safety-state write fails and deletion could follow, the client does not proceed. Progress writes remain best-effort (observational only).

### Subprocess heartbeat interval

The heartbeat interval for `step_running` progress updates is configurable through an internal parameter. Production default is 5 seconds.

## Subprocess argv hardening

Purgery hardens its own `ssh` and `rsync` argv with a literal `--` separator:

- `ssh -- HOST COMMAND` prevents the host value from being parsed as an ssh option.
- `rsync [options...] -- SOURCE DESTINATION` prevents source and destination paths from being parsed as rsync options.

Postprocess commands are trusted server-side configuration. Purgery does not rewrite, validate, or auto-fix postprocess argv.
