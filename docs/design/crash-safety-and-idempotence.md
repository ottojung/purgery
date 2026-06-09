# Crash Safety and Idempotent Imports

This document defines the filesystem invariants that make Purgery restart-safe. The [import semantics](import-semantics.md) describe per-file processing details; this document describes the durable state machine around them.

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
```

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

The client may delete a local file only after reading a valid `status.toml` whose envelope matches the uploaded manifest:

```text
status.nickname == manifest.nickname
status.run_id == manifest.run_id
```

No status means no deletion. A file is recorded as `imported` only after all outputs for that file have been committed. Run states mean:

- `done`: every file was imported;
- `partial`: at least one file was imported and at least one failed or was skipped;
- `failed`: no files were imported.

The server commits final outputs before atomically publishing successful status. A crash before status publication therefore cannot authorize client deletion.

## Idempotent import invariant

Uploading the same logical destination again replaces the existing regular final file. The server does not remember whether it has seen a file and has no deduplication database.

With deterministic postprocessing, importing the same input and destination repeatedly converges to the same final content and is a semantic no-op. Non-deterministic postprocessing may produce different content on a later import; replacing the prior regular file with that output is allowed.

This property also makes crash recovery replay-based. A crash may occur after some final outputs have been replaced but before `status.toml` is written. The client retains its local files because no success status exists. On restart, the server replays the processing run and atomically replaces those outputs again, converging on the run's result.

## Replacement invariant

Each output is copied to a temporary file in its final directory and then renamed to the final path:

```text
work output -> final parent/.purgery-commit.<run_id>.<filename>.tmp -> final path
```

On Unix, the final rename atomically creates a missing destination or replaces an existing regular file. Purgery does not restore old final contents when a later output in the same file fails; the run remains recoverable and is replayed if it did not publish status.

Purgery refuses an output when the final path:

- is an existing directory;
- is a symlink, including a dangling symlink;
- crosses a symlink in a parent component;
- is another non-regular filesystem object; or
- escapes the configured storage root.

## No implicit delete invariant

A run affects only outputs it explicitly commits. Purgery does not use `rsync --delete`, and it does not infer deletion from an output being absent in a later run. If an earlier run produced extra outputs that a later run does not produce, those earlier outputs remain in final storage.

## Client crash matrix

| Crash point | Durable result and restart behavior |
|---|---|
| Before `begin-run` | No server state exists. |
| During upload | The run remains in `incoming/`; its lease and garbage collection handle abandonment. |
| After `finish-run`, before status | The run is durable in `ready/` or `processing/`. Rerunning the client may upload the same files again, which is safe. |
| After server import, before cleanup | A valid server status exists, but the client may upload again after restart. Atomic replacement makes the repeated import safe. |
| After cleanup | Confirmed local files are gone. If a file is later re-created at the same local path, importing it again safely replaces the regular final file. |

The client keeps no local run database. Verified server status remains the sole authority for local deletion.
