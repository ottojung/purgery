# Import Semantics

## Storage location invariants

Purgery maintains two distinct storage locations:

* **`root`** (final archive): Output-only storage. The only paths Purgery may create or modify under `root` are the actual final imported files, directories, and symlinks and their final postprocessed output versions. No operational, temporary, intermediate, progress, status, lock, staging, partial, or helper files are ever created under `root`.

* **`purgery_root`** (Purgery-owned operational state): All operational state lives here, including incoming runs, ready/processing/done/failed run directories, manifests, status/progress files, work areas, postprocess staging, temporary commit helpers, and any staging needed for safe commits.

The rule is: nothing non-final may appear under `root`. A partially written exact final file path is allowed during interrupted transfer or materialization — the path is still the actual final file being transferred. A sibling helper path (such as `.purgery-commit.<run_id>.<filename>.tmp`) is not, because it is not a final user-data path. Sibling helper paths under `root` are forbidden regardless of naming convention.

## Commit path by output kind

### Regular files and symlinks

Committed directly from their work-area location to the final path under `root`:

```
work output → direct copy to final path
```

The source (staged file or work-area output) is a complete file already verified against the manifest. A crash during the copy to final storage may leave a partial file at the exact final path. This is acceptable because the run has not published `status.toml` and will be replayed from staged data on recovery, overwriting the partial file.

No sibling temp file (such as `.purgery-commit.*.tmp`) is created in the final parent directory during commit. The copy writes directly to the final path.

### Directory roots

Directory output roots are created, kept, or replaced directly via `commit_directory_entry`. Their descendants are then recursively overlaid using no-delete semantics. Subdirectories are created/kept directly; regular-file and symlink descendants are committed directly from their work-area sources to their final paths.

## Directory overlay semantics

Purgery uses recursive no-delete overlay semantics for commits. Existing directories are kept and merged. Regular files and symlinks replace existing conflicting entries (files, symlinks, or empty directories). Non-empty directories are not replaced — the operator must resolve them.

Commits are not all-or-nothing. A crash during commit may leave some outputs already written to final storage, and a crash during a direct file copy may leave the exact final path with partial contents. This is acceptable because `status.toml` has not been published yet, `processing/` still exists, and `process-once` replays from staged files with idempotent commits, overwriting any partial remnants.

## Rsync and `--partial`

All rsync invocations include `--partial`. This ensures that interrupted transfers (whether to staging or directly to final storage) can resume without re-transferring already-received data. A partially transferred file at an exact final path is still the actual final file being transferred — it is not an operational helper path.

The invariant is that `root` contains only final user-data paths, not that every byte under `root` is always complete during transfer. Operational files (status, progress, lock, staging, filter, cleanup, or commit-helper files) must never appear under `root` under any circumstance.

## `final_paths` (plural)

Status entries use `final_paths` — a list of all committed paths relative to the server root. A single-output import produces one entry. Postprocessing (e.g., `compress-video`) may produce multiple outputs (original + compressed).

For a failed entry, `final_paths` is empty and the `error` field contains a description.

## Per-entry errors

Per-entry failures produce individual `EntryStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining entries. Only truly catastrophic errors (unreadable run config, invalid match pattern, missing step reference, unparseable manifest, envelope mismatch) abort the entire run.

## Work area

The server creates a work area at `<purgery_root>/<nickname>/processing/<run_id>/work/`. Entries are placed into the work area before processing. All staging, temporary files, helper paths, and intermediate artifacts live under this work area, never under `root`.

Postprocess subprocesses run with their current directory set to the work-area parent of the input entry. This is work-area discipline: it makes relative-path outputs land inside the work area, not in an arbitrary inherited server cwd. Purgery validates that expected output paths resolve under the work area before committing them to final storage. This is not a security sandbox — a malicious subprocess can still write anywhere the process has permissions.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

Stale work areas from interrupted runs are removed on processing start. The work area is rebuilt from staged files for each processing attempt.

## Run plan validation

Before processing any entries, the server validates the run plan via `prepare-run`. This validates the manifest classification, match patterns, step references, expected-output names, and planned final paths. If anything is invalid, the run is rejected before any passthrough rsync mutates final storage.

All server-side rule matching is sync-scoped. Every check that tests whether a rule matches an entry also verifies that the rule applies to the entry's sync group via `rule.applies_to(entry_sync_name)`. A rule scoped to sync group A never affects entries from sync group B, even if the pattern matches.

## Passthrough architecture

Ordinary passthrough entries have no server bookkeeping. They are transferred directly to final server storage by bulk rsync. The uploaded server manifest contains only postprocess roots and covered descendants — ordinary passthrough entries are excluded.

### Identity bookkeeping boundaries

For ordinary passthrough entries with `delete_after_import = false`:

- Path planning (sync name, relative path, kind, classification) is computed for rsync filter generation
- Size is read from filesystem metadata only to determine file type for classification
- mtime_ns and SHA-256 are never computed
- No cleanup state is written
- The only operation is direct rsync

For ordinary passthrough entries with `delete_after_import = true`:

- Identity is captured per entry kind: size, mtime_ns, and SHA-256 for regular files; link target for symlinks; existence and captured descendants for directories. Regular files without SHA identity are not deletion-authorizing.
- Durable cleanup state is written atomically to the configured `state_dir`
- Cleanup verifies local identity before removal

### Durable cleanup state

The cleanup state is stored in the client's `state_dir` (a required absolute path in `client.toml`). It is never stored in a temporary directory. For a sync group with `delete_after_import = true`, the client writes a durable cleanup state file atomically before rsync, with `rsync_succeeded = false`. After rsync succeeds, the success marker is atomically updated to `true`. The state records identity per entry kind (size, mtime, SHA-256 for regular files; link target for symlinks; subtree entries for directories). The cleanup state is replayable and idempotent: already-removed entries are safe, changed entries are skipped, and entries lacking required identity are left untouched.

After each successful deletion, the cleanup state is rewritten atomically (temp file + rename). A crash during cleanup does not make progress ambiguous: already-deleted entries are idempotent, pending entries are retried.

If no sync group has any postprocess roots, no server run is created. The client uses a side-effect-free `resolve-destinations` server command to obtain final storage paths, then rsyncs directly.

## Sync group classes

Every sync group is one of two classes determined at config validation time.

The configured `sync.from` path is the source tree root — a traversal boundary and configuration anchor. It is not itself an imported entry. Manifests, cleanup state, and cleanup operations cover entries **under** the source root, not the root directory itself.

- **Passthrough group**: no applicable postprocess rules. `delete_after_import` may be true or false. The group is handled entirely outside the purgatory lifecycle.
  - `delete_after_import = false`: one direct unfiltered rsync, no walk, no cleanup state.
  - `delete_after_import = true`: one direct unfiltered rsync plus a durable cleanup ledger. Cleanup identity is captured before rsync. After rsync succeeds, rsync_succeeded is durably set and entries whose pre-rsync identity still matches are removed. No per-entry transfer filters, no server manifest entries.
- **Purgatory group**: one or more applicable postprocess rules and `delete_after_import = true`. The group participates in walking, manifest building, upload, and server processing.

Passthrough groups are not included in the uploaded run config, server manifest, or status. In mixed invocations, passthrough destinations are resolved separately through the side-effect-free `resolve-destinations` command. The purgatory transfer loop iterates only purgatory groups.

If a sync group has applicable postprocess rules but `delete_after_import = false`, config validation rejects it before any filesystem walking.

## Transfer model

The client generates transfer sets per sync group according to its class:

For purgatory groups:

1. **Passthrough transfer set**: exact-path roots for entries with mode `passthrough` (regular files, symlinks, empty directories). Transferred directly to final storage.
2. **Purgatory transfer set**: exact-path roots for ordinary postprocess entries plus subtree roots for postprocessed directories. Transferred to the server's staging area (`<purgery_root>/<nickname>/incoming/<run_id>/files/<sync.to>/`). Postprocess entries remain in staging until the server processes them successfully; only then are they committed to final storage. If processing fails, the run fails and no outputs reach the final archive.

For passthrough groups, the entire source tree is transferred via one direct unfiltered rsync to final storage. No transfer sets, no manifest, no server bookkeeping.

If a transfer set is empty, the corresponding rsync call is skipped entirely.

### Exact path roots

For ordinary entries (passthrough or postprocess but not covered):
- Regular files and symlinks transfer as independent entries.
- Empty directories transfer as independent entries.
- Non-empty directories are traversal containers unless selected as postprocess roots.

### Subtree roots

A directory selected for postprocessing is a subtree transformation root:
- The directory entry has mode `postprocess`.
- All descendants have mode `covered`.
- Covered descendants are not independent transfer roots.
- The purgatory rsync filter includes the entire directory subtree.
- The server processes the directory root as one postprocess input.
- Covered descendants are skipped in status and are not independently cleaned from server status. They are retired as part of the postprocessed directory root's all-or-nothing cleanup when the root subtree is verified and removed bottom-up.

## Postprocessing applies to all entry kinds

Every manifest entry kind (directory, regular file, symlink) is eligible for postprocessing. If an entry's normalized path matches a postprocess rule, the entry is transformed by the subprocess. If it does not match any rule, the entry is imported directly.

### Work-area preparation

Before a subprocess runs, the server creates an isolated work-area representation of the matched entry:

* **Regular file**: copied from the staged file.
* **Symlink**: created in the work area with the same literal target. Symlinks are never followed.
* **Directory**: the entire staged subtree is copied into the work area, preserving directories, regular files, and symlinks as symlinks. Unsupported filesystem objects inside the subtree fail the directory entry.

### Directory transform boundary

If a directory entry matches a postprocess rule, that directory becomes a transformation boundary. Its descendant manifest entries are marked as **covered** and are not imported independently. They produce a `skipped` status entry with `"covered by postprocessed ancestor directory"`.

## Postprocess outputs

Expected outputs are file-name templates for output entry roots in the same work-area parent directory as the input entry. Allowed placeholders: `{file_name}`, `{file_stem}`, `{stem}`. `{input}` and `{parent}` are forbidden in expected outputs.

After a subprocess runs, each expected output path is inspected with `symlink_metadata` to determine its kind:
* **Directory**: committed recursively using no-delete overlay semantics.
* **Regular file**: committed from its work-area location directly to the final path.
* **Symlink**: the symlink target is read and the symlink is recreated at the final path.
* **Unsupported or missing**: input entry fails.

`keep_original = true` commits the original work-area input entry/root as one output. For directories this commits the subtree recursively.

After collection, outputs are deduplicated while preserving order.

The status `postprocess` field is derived by matching rules against the logical normalized path (e.g., `videos/video.mp4`), not the absolute work-area path.

## Malformed status handling

If a `status.toml` exists but is malformed (invalid TOML or missing required fields), the status command returns a parse error rather than silently skipping it.

## Directory layout

```
<root>/                              # output-only final archive storage
  <nickname>/
    <sync.to>/                       # final imported entries, postprocessed outputs
      ...                            # (only final files/dirs/symlinks — no temp/helper files)

<purgery_root>/                      # Purgery-owned operational state
  <nickname>/
    incoming/<run_id>/               # client uploads here
      lease.toml
      run.toml
      manifest.toml
      files/                         # uploaded entries by sync mapping (staging)
    ready/<run_id>/                  # upload complete, pending processing
    processing/<run_id>/             # actively being processed
      work/                          # per-run work area (staging, temp commit helpers,
      |                              #   postprocess inputs/outputs, intermediate artifacts)
      status.toml
      progress.toml
      run.toml
      manifest.toml
      files/                         # staged entry files
    done/<run_id>/                   # successfully processed (work/ removed if Done)
      status.toml
      run.toml
      manifest.toml
      ...
    failed/<run_id>/                 # failed runs (work/ preserved for diagnostics)
      status.toml
      run.toml
      manifest.toml
      ...
```

## Postprocessing conformance and import-and-retire

Postprocessing requires `delete_after_import = true`. This is an intentional conformance tradeoff, not an arbitrary safety constraint.

Purgery does not retain indefinite source-entry metadata on the server. After a run completes, the server only keeps a bounded run status — it does not maintain a permanent record of every source entry's identity, fingerprints, or processing history.

For postprocessed imports, the archive does **not** contain the original source entry. It contains transformed outputs: compressed files, converted files, renamed outputs, generated directories, or whatever the postprocess step produced. Purgery cannot use the archive alone to answer whether an unchanged local original:

* was already processed;
* was processed with the same rule set;
* was processed with the same step definitions;
* produced the same expected outputs;
* should be skipped or reprocessed;
* or represents a changed source that happens to map to the same archive destination.

Solving this would require persistent server-side source fingerprints, retained manifests, retained source metadata, or an indefinitely growing receipt ledger. Purgery explicitly chooses not to have that model.

Therefore, postprocessing is modeled as an import-and-retire operation:

1. The source entry is uploaded into a server run.
2. The server transforms and commits outputs.
3. The server writes a bounded run status.
4. The client removes the unchanged local original entry after server-confirmed import.

The source original is removed from the source tree after successful import, so it will not be repeatedly reprocessed by later runs.

## Planned final-path validation

A run is rejected before entry processing if planned final paths conflict, including direct manifest-entry paths and postprocess-derived output roots.

## Cleanup authority by entry type

There are two distinct cleanup authorities:

1. **Server-confirmed cleanup** — applies to postprocess/transformed entries. Local deletion is authorized by a valid server status file.
2. **Transfer-confirmed cleanup** — applies to passthrough entries with `delete_after_import = true`. Local deletion is authorized by a durable local cleanup state file.

### Postprocess entries (server-confirmed cleanup)

Only manifest entries with mode `Postprocess` are eligible for server-confirmed cleanup.

For postprocess entries, cleanup authority is the server's `status.toml`:

1. The server's `status.toml` is valid and parseable.
2. `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`.
3. The manifest entry mode must be exactly `Postprocess`. Entries with mode `Covered` or `Passthrough` are not eligible, even if a status entry incorrectly reports them as `imported`.
4. The entry's own status is `imported`.
  5. The local entry still matches its captured identity:
     * **regular files**: size, mtime, and SHA-256 must all match; path must still be a regular file. Missing SHA prevents deletion.
     * **symlinks**: literal link target string must match; path must still be a symlink; target is never followed.
     * **directories**: path must still be a directory; every present captured descendant must still match its identity; absent captured descendants are treated as already removed; no new or changed entries may exist anywhere under the root.
6. The sync mapping has `delete_after_import = true`.

Covered descendants are not independently cleaned from server status. They are retired as part of the postprocessed directory root's all-or-nothing cleanup when the root subtree is preflighted and removed bottom-up.

### Passthrough entries (transfer-confirmed cleanup)

For passthrough entries, cleanup authority is the durable local cleanup state:

1. A valid cleanup state file exists on disk with a recorded rsync success marker (i.e. `rsync_succeeded = true`).
2. The cleanup identity was captured **before** rsync — either from a pre-rsync source walk (PassthroughDeleteAfterImport) or from the pre-rsync manifest (purgatory passthrough remainder).
  3. The local entry still matches the recorded identity for its kind:
     * **regular files**: size, mtime, and SHA-256 must all match. Missing SHA prevents deletion.
     * **symlinks**: literal link target must match; symlink is unlinked without following the target.
     * **directories**: captured descendants must still match; no new entries may exist inside; removal is bottom-up.
4. The sync mapping has `delete_after_import = true`.

The cleanup state is always written before the passthrough rsync, with `rsync_succeeded = false`. Deletion is authorized only after rsync succeeds and the success marker is durably recorded.

Passthrough entries with `delete_after_import = false` are never cleaned.

### General properties

Deletion is idempotent. If a captured entry is already absent at cleanup time, it is treated as already removed (provided the cleanup authority exists). This applies to regular files, symlinks, and directories. If the entry is present but changed since upload, the client leaves it untouched.

Cleanup identity is checked per entry kind:

- **Regular files**: size, mtime, and SHA-256 must all match. Missing SHA prevents deletion.
- **Symlinks**: the literal link target string must match. The symlink is unlinked without following the target. The target path itself is never modified.
- **Directories**: the directory must exist and its tracked descendants must still match their captured identities. Directories are removed bottom-up: child entries are removed first, then the directory itself. If new or changed entries appeared inside the directory after identity capture, the directory is left in place. If any regular-file descendant lacks SHA identity or SHA recomputation fails, the directory is not removed.

## Diagnostics policy

Production/library code must use `tracing` macros (`tracing::warn!`, `tracing::info!`, etc.) for diagnostics.

Production code must not contain `println!`, `eprintln!`, or `dbg!` calls. These are for temporary debugging only and must be removed before committing.

Cleanup-safety diagnostics (missing identity, identity mismatch, recomputation failure) should use `tracing::warn!` with structured context so they are visible during normal operation without relying on stderr capture.
