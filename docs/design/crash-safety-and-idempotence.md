# Crash Safety and Idempotent Imports

This document defines the filesystem invariants that make Purgery restart-safe. The [import semantics](import-semantics.md) describe per-entry tree-overlay details; this document describes the durable state machine around them.

## Client cleanup state

For passthrough entries with `delete_after_import = true`, the client writes a durable cleanup state file at a stable location:

```text
{state_dir}/
```

The location is the required `state_dir` field in `client.toml`, which must be a non-empty absolute path.

This is the only cleanup mechanism. It is used for all delete-after-import cleanup: pure passthrough invocations, mixed invocations, and purgatory passthrough remainder.

The cleanup ledger protocol is the same everywhere:

1. Capture cleanup identity **before** rsync — either by walking the source (PassthroughDeleteAfterImport) or from the pre-rsync manifest (purgatory passthrough remainder).
2. Write cleanup state atomically with `rsync_succeeded = false`.
3. Run the passthrough rsync.
4. If rsync succeeds, atomically mark `rsync_succeeded = true`.
5. Delete only entries whose current local identity still matches the pre-rsync capture.
6. Atomically mark each successfully deleted entry as cleaned.

If rsync fails, deletion is not authorized. The `rsync_succeeded` flag remains `false` and resume does not delete.

If the client crashes before `rsync_succeeded = true` is durably recorded, cleanup must not run.

If the client crashes after `rsync_succeeded = true`, the next invocation may resume cleanup safely.

For purgatory passthrough remainder, the pre-rsync identity comes from the manifest entries (built during the pre-transfer walk). The cleanup state is written before the passthrough rsync, not after.

### Atomicity

The cleanup state is written atomically via temporary file + rename. After each successful local deletion, the cleanup state is rewritten atomically so that a crash during cleanup does not make progress ambiguous.

### Replay/idempotence

On a later `sync-and-cleanup` invocation, the client scans the cleanup state directory for pending cleanup. Already-removed entries are idempotent (not found = success). Changed entries are skipped (identity check fails). Pending entries are retried.

## Statelessness invariant

Purgery has no durable hidden database. Its durable state is the filesystem:

```text
incoming/
ready/
processing/
done/
failed/
lease.toml
run.toml
manifest.toml
status.toml
work area
final storage
client passthrough cleanup state (local, for delete_after_import passthrough)
client postprocess run state (local, for resumable postprocess waiting/cleanup)
```

The client persists postprocess run metadata under `state_dir`:

```text
{state_dir}/runs/{nickname}-{run_id}/state.toml
```

This file contains the manifest, run config, and local phase. It is written before `finish-run` and updated as the run progresses through waiting, cleanup, and completion. It allows the client to resume waiting for a postprocess run after a crash without requiring a fresh upload.

Passthrough cleanup state and postprocess run state are separate:

* Passthrough cleanup state (`cleanup-*.toml` in `state_dir/`) handles transfer-confirmed deletion of passthrough entries.
* Postprocess run state (`state_dir/runs/{nickname}-{run_id}/`) handles server-confirmed deletion of postprocess entries.

Client and server processes may stop at any instruction. Recovery must follow from these files and directories alone, without remembered process state. Metadata files are written through temporary files and renamed into place where applicable.

## Phase-state invariant

A run's phase directory is its durable state machine:

```text
incoming -> ready -> processing -> done
                              \-> failed

incoming -> failed   # garbage collection of an abandoned upload
```

Phase transitions use rename so a run is claimed or finalized as one filesystem operation when the source and destination share a filesystem. `process-once` examines both `processing/` and `ready/`: interrupted processing runs are recovered before new ready runs are claimed.

## Processing recovery

A run in `processing/` is handled according to its filesystem state:

- A valid `status.toml` whose nickname and run ID match the directory is terminal. The server completes the pending rename to `done/` for `done` or `partial`, or to `failed/` for `failed`.
- A missing `status.toml` means processing was interrupted. The server removes the run's work area best-effort, rebuilds it from staged files, and replays the run.
- A malformed `status.toml` cannot establish success. The server replaces it with a failed status carrying `interrupted processing had malformed status` and moves the run to `failed/`.
- A status whose nickname or run ID does not match the processing directory cannot establish success. The server replaces it with a failed status carrying `interrupted processing had mismatched status envelope` and moves the run to `failed/`.

The staged files in the processing directory are the replay source. The work area is disposable and is rebuilt for every attempt. Recovery reports an error if it cannot atomically publish the failed status or move the run to `failed/`; it never reports successful recovery while the run remains non-terminal.

## Status and deletion invariant

Postprocessing is import-and-retire. Because the server does not retain indefinite source-entry metadata, the confirmed local original is removed after successful import to prevent repeated reprocessing. See [import semantics](import-semantics.md#postprocessing-conformance-and-import-and-retire).

### Postprocess entries (server-confirmed cleanup)

The client may remove a local postprocessed entry only after reading a valid `status.toml` whose envelope matches the uploaded manifest:

```text
status.nickname == manifest.nickname
status.run_id == manifest.run_id
```

No status means no deletion. An entry is recorded as `imported` only after all of its outputs have been committed. Run states mean:

- `done`: every entry was imported;
- `partial`: at least one entry was imported and at least one failed or was skipped;
- `failed`: no entries were imported.

The server commits final outputs before atomically publishing successful status. A crash before status publication therefore cannot authorize client cleanup of local entries.

### Passthrough entries with delete_after_import=true

The client may remove a local passthrough entry only after a durable cleanup state file on disk records that rsync succeeded. The cleanup state is written atomically (via temporary file + rename) before rsync, with `rsync_succeeded = false`. After rsync succeeds, the success marker is atomically updated to `true`. A crash before the initial write leaves no cleanup state, so removal is not authorized. A crash between the initial write and the success marker prevents removal because `rsync_succeeded` remains `false`.

The client verifies local identity against the cleanup state before removing. Entry-kind identity checks apply: size, mtime, and SHA-256 for regular files; link target for symlinks; subtree identity for directories. Missing required identity prevents deletion. Changed entries are skipped. Already-removed entries are idempotent.

## Idempotent tree-overlay invariant

Uploading the same logical tree again replays the same directory, regular-file, and symlink entries. The server keeps no deduplication database. Existing directories are retained and merged, regular files and symlinks are replaced according to the characterized rsync rules, and unrelated final descendants remain.

With deterministic postprocessing, importing the same input tree repeatedly converges to the same final tree and is a semantic no-op. Non-deterministic postprocessing may produce different regular-file content on a later import; replacing the prior output is allowed.

This property makes crash recovery replay-based. A crash may occur after some entries have committed but before `status.toml` is written. The client retains local entries because no success status exists. On restart, the server replays the processing run from staged entries and converges on the run's result.

## Per-entry replacement invariant

Regular-file outputs are copied to a temporary file in their final directory and renamed into place. Symlinks are created at a temporary name in their final directory and renamed into place. Directories are created or retained directly:

```text
regular work output -> final parent/.purgery-commit.<run_id>.<filename>.tmp -> final path
literal link target -> final parent/.purgery-commit.<run_id>.<filename>.tmp -> final symlink
```

A present source directory replaces a conflicting final file or symlink and then allows descendants to merge. A present source regular file or symlink replaces a final file, symlink, or empty directory, but fails rather than deleting a non-empty final directory. Existing ancestors must be real directories; final-storage symlinks are never followed as directory components. Every derived path must remain inside the configured storage root.

A later failure does not restore entries already committed by the same run. The run remains recoverable and is replayed if it did not publish terminal status.

## No implicit delete invariant

A run affects only outputs it explicitly commits. Purgery does not use `rsync --delete`, and it does not infer deletion from an output being absent in a later run. If an earlier run produced extra outputs that a later run does not produce, those earlier outputs remain in final storage.

## Client crash matrix

### Postprocess run crash matrix

| Crash point | Durable result and restart behavior |
|---|---|
| Before local state written | Upload not yet complete; no server-side finished run exists. |
| After upload complete, before `finish-run` | Local state written as `upload_complete_finish_pending`. Resume checks server phase: if `incoming`, re-runs `finish-run`; if later phase, proceeds to waiting; if `not_found`, marks abandoned; if `corrupt`, marks corrupt. |
| After `finish-run` accepted, before terminal status | Local state is `waiting_for_terminal_state`. Resume calls `run-state` and continues waiting (ready, processing) or handles terminal/corrupt/not_found. No new upload is needed. |
| While waiting for terminal state | Resume continues waiting for the same run. Only `ready` and `processing` justify indefinite waiting. |
| Transport failure while waiting | `run-state` command fails. Local state preserved unchanged (e.g. `WaitingForTerminalState`). Invocation returns error. No infinite retry. |
| Malformed `run-state` response | Response is unparseable TOML. Local state preserved. Invocation returns error. No infinite retry. |
| `run-state` returns `incoming` (while in `WaitingForTerminalState`) | Protocol inconsistency. Local state preserved unchanged. Invocation returns error. No infinite retry. No deletion. |
| `run-state` returns `corrupt` | Server has terminal-phase directory but invalid status. Client writes `ClientRunPhase::Corrupt` tombstone. No deletion. Invocation returns error. |
| `run-state` returns `not_found` | Server has no record of this run. Client writes `ClientRunPhase::Abandoned` tombstone. No deletion. Invocation returns error. |
| After terminal `run-state`, before status | Client preserves `TerminalStatusSeen` phase. |
| Status command fails (transport) | Local state is `TerminalStatusSeen`. Invocation returns error preserving this phase. No infinite retry. No deletion. |
| Status command returns malformed | Client writes `ClientRunPhase::Corrupt` tombstone. No deletion. |
| Status envelope mismatches | Client writes `ClientRunPhase::Corrupt` tombstone. No deletion. |
| After terminal status seen, before cleanup | Local state is `terminal_status_seen`. Resume goes **directly** to terminal status verification/cleanup — does not rewrite to `WaitingForTerminalState` and does not call the wait loop. |
| After partial cleanup | Resume repeats cleanup idempotently (already-removed entries are safe, identities are rechecked). |
| After cleanup complete | Local state cleaned up. |
| Abandoned/lost | Local state is `abandoned`. No deletion authorised. State remains as durable diagnostic tombstone until explicitly cleared. Blocks new sync. |
| Server state corrupt | Terminal phase directory exists but status is missing, malformed, or mismatched. Local state is `corrupt`. No deletion. Durable tombstone. Blocks new sync. |

### Passthrough-specific crash matrix (pure passthrough groups)

| Crash point | Durable result and restart behavior |
|---|---|
| Before cleanup state write | No cleanup state exists. |
| After cleanup state written (rsync_succeeded=false), before rsync | Cleanup state exists but rsync has not run. Restart skips cleanup (rsync_succeeded is false). |
| During passthrough rsync | The rsync may partially transfer entries. Rerunning the client rsyncs again (idempotent). |
| After rsync, before success marker update | Entries were transferred but cleanup is not authorized (rsync_succeeded is still false). Restart re-runs rsync (idempotent). |
| After success marker (rsync_succeeded=true), before deletion | Cleanup state authorizes removal. Restart reads cleanup state from stable directory and resumes deletion. |
| After some deletions, before cleanup state updated | Already-removed entries are idempotent (not found = OK). Remaining entries are removed after identity check. Next cleanup state write atomically updates progress. |
| After cleanup state updated, all deletions complete | Cleanup state marks all entries as cleaned. Restart sees no pending cleanup. |

### Abandoned/corrupt tombstones block new sync

Normal `sync-and-cleanup` must not start new sync work while any `Abandoned` or `Corrupt` postprocess tombstone exists under `state_dir/runs/`. This prevents silent data loss from an unresolved server-side corruption.

The client scans for tombstones during startup resume, before any new scan, rsync, or server interaction. If found, the invocation returns an error indicating the tombstone path. The user must manually clear the tombstone (remove the directory) or use a future explicit command.

Tombstones are durable diagnostics. They are never auto-removed.

### TerminalStatusSeen resume bypasses wait loop

When resuming from `TerminalStatusSeen`, the client must not re-enter the `wait_for_postprocess_run_and_cleanup` wait loop. It must go directly to reading terminal status and performing cleanup. This prevents rewriting state to `WaitingForTerminalState` when the run is already past waiting.

### Processing progress timestamp semantics

Progress timestamps have two distinct fields:

- `started_at_unix_secs`: The stable wall-clock time when the server began processing this run. It is set once on the first progress write and preserved across all subsequent writes. If an existing `progress.toml` is found whose envelope (`nickname`, `run_id`) matches the current run, `started_at` is read from it rather than recomputed. If the existing file is missing, malformed, or envelope-mismatched, a fresh `started_at` is initialized to the current time. This value never changes for a given run.
- `updated_at_unix_secs`: The wall-clock time of the current progress update. It is always set to `now` and never preserved from a prior update.

The first progress write for a run initializes both fields to the current time. All subsequent writes preserve `started_at` and update only `updated_at`. This allows observers to distinguish run duration from staleness.

### Run-level vs per-entry progress

Progress states are classified:

| Type | States |
|------|--------|
| **Run-level** | `processing_started`, `publishing_status` |
| **Per-entry** | `processing_entry`, `step_started`, `step_running`, `step_finished` |

Run-level progress has empty `current_entry` and `current_step`. This is not a sentinel — it means the progress describes the run as a whole, not a specific entry. Per-entry progress always has real `entry_index`, `entry_total > 0`, and non-empty `current_entry`.

### Progress write failure

Progress is observational only. It never authorizes cleanup. A progress file write failure is warning-level and must not fail an otherwise successful import.

The server writes `publishing_status` progress before atomically publishing terminal `status.toml`. If this write fails, processing continues — it is best-effort.

Progress updates never use sentinel values. Every per-entry progress update (`processing_entry`, `step_started`, `step_running`, `step_finished`) carries accurate `entry_index`, `entry_total`, and `current_entry`. No field is left as `0` to mean "the caller should fill this in later."

All progress writes use a best-effort helper that logs a `tracing::warn!` on failure with structured context. Silent `let _ = write_progress(...)` patterns are replaced with explicit warning-level logging.

### Tombstone persistence failure errors

When an `Abandoned` or `Corrupt` tombstone write fails, the returned error combines:

- the original condition (`not_found`, server `corrupt`, malformed terminal status, envelope mismatch);
- the fact that the tombstone could not be persisted.

The client still does not delete anything. The error message directs the user to the state directory.

### Safety-state persistence

Client-persisted run state (under `state_dir/runs/`) follows different rules from progress:

- **`WaitingForTerminalState`**: Persisted before entering the wait loop. Failure stops before waiting.
- **`TerminalStatusSeen`**: Persisted before reading server status. Failure stops before potential deletion.
- **`CleanupComplete`**: Persisted after successful deletion. Failure leaves old state in place for recovery.
- **`Abandoned`** / **`Corrupt`** (tombstones): Persisted before returning error. If the tombstone write fails, the client returns an error that says the tombstone could not be persisted, and still does not delete.

Safety-state writes are not best-effort. If a safety-state write fails and deletion could follow, the client does not proceed. Progress writes remain best-effort.

### Cleanup state discovery

On startup, the client scans the passthrough cleanup state directory (`state_dir`) for state files with pending cleanup. If found, cleanup is resumed before any new rsync operation. Separately, the client scans `state_dir/runs/` for pending postprocess run state and resumes waiting/cleanup before any new sync work.

For postprocess entries, verified server status remains the authority for local deletion. The local run state provides crash recovery for the waiting/cleanup handshake. For passthrough entries with `delete_after_import=true`, the durable local cleanup state is the authority.

## Tree-overlay recovery guarantee

Purgery provides replayable convergence, not an all-or-nothing tree transaction. A crash may leave some directories, regular files, or symlinks committed while later entries remain pending. Until every manifest entry completes, `status.toml` is not published and the run remains recoverable in `processing/`. `process-once` rebuilds regular-file work outputs from immutable staged data and replays every entry. Directory merge, same-directory regular-file replacement, and same-directory symlink replacement are idempotent, so replay converges while preserving unrelated final descendants.

Per-entry commits are crash-safe and replay-safe. A terminal success status is published only after the complete manifest has been processed.
