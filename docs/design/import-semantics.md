# Import Semantics

## Commit path by output kind

### Regular files and symlinks

Committed through a same-directory temp entry followed by atomic rename:

```
work output → final parent dir / .purgery-commit.<run_id>.<filename>.tmp → rename → final path
```

The temp entry is on the same filesystem as the final path, so the rename is atomic against readers. Temp entries are cleaned up after a successful commit.

### Directory roots

Directory output roots are created, kept, or replaced directly via `commit_directory_entry`. Their descendants are then recursively overlaid using no-delete semantics. Subdirectories are created/kept directly; regular-file and symlink descendants use temp-entry + rename.

## Directory overlay semantics

Purgery uses recursive no-delete overlay semantics for commits. Existing directories are kept and merged. Regular files and symlinks replace existing conflicting entries (files, symlinks, or empty directories). Non-empty directories are not replaced — the operator must resolve them.

Commits are not all-or-nothing. A crash during commit may leave some outputs already written to final storage. This is acceptable because `status.toml` has not been published yet, `processing/` still exists, and `process-once` replays from staged files with idempotent commits.

## `final_paths` (plural)

Status entries use `final_paths` — a list of all committed paths relative to the server root. A single-output import produces one entry. Postprocessing (e.g., `compress-video`) may produce multiple outputs (original + compressed).

For a failed entry, `final_paths` is empty and the `error` field contains a description.

## Per-entry errors

Per-entry failures produce individual `EntryStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining entries. Only truly catastrophic errors (unreadable run config, invalid match pattern, missing step reference, unparseable manifest, envelope mismatch) abort the entire run.

## Work area

The server creates a hidden work area at `<root>/.purgery-work/<nickname>/<run_id>/`. Entries are placed into the work area before processing.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

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

For ordinary passthrough regular files with `delete_after_import = true`:

- Size, mtime_ns, and optional SHA-256 are computed
- Durable cleanup state is written atomically to the stable state directory
- Cleanup verifies local identity before deletion

### Durable cleanup state

The cleanup state is stored in the client's state directory, defaulting to `$XDG_STATE_HOME/purgery/` or `~/.local/state/purgery/` (configurable via `state_dir` in the client config). It is never stored in a temporary directory. For a sync group with `delete_after_import = true`, the client writes a durable cleanup state file atomically after confirming rsync success. This state records the file identity (size, mtime, optional SHA-256) and is used on restart to safely delete confirmed files. The cleanup state is replayable and idempotent: already-deleted files are safe, changed files are skipped.

After each successful deletion, the cleanup state is rewritten atomically (temp file + rename). A crash during cleanup does not make progress ambiguous: already-deleted entries are idempotent, pending entries are retried.

If no sync group has any postprocess roots, no server run is created. The client uses a side-effect-free `resolve-destinations` server command to obtain final storage paths, then rsyncs directly.

## Sync group classes

Every sync group is one of two classes determined at config validation time:

- **Passthrough group**: no applicable postprocess rules. `delete_after_import` may be true or false. The group is handled entirely outside the purgatory lifecycle.
  - `delete_after_import = false`: one direct unfiltered rsync, no walk, no cleanup state.
  - `delete_after_import = true`: one direct unfiltered rsync plus a durable cleanup ledger. Cleanup identity is captured before rsync. After rsync succeeds, rsync_succeeded is durably set and files whose pre-rsync identity still matches are deleted. No per-entry transfer filters, no server manifest entries.
- **Purgatory group**: one or more applicable postprocess rules and `delete_after_import = true`. The group participates in walking, manifest building, upload, and server processing.

Passthrough groups are not included in the uploaded run config, server manifest, or status. In mixed invocations, passthrough destinations are resolved separately through the side-effect-free `resolve-destinations` command. The purgatory transfer loop iterates only purgatory groups.

If a sync group has applicable postprocess rules but `delete_after_import = false`, config validation rejects it before any filesystem walking.

## Transfer model

The client generates transfer sets per sync group according to its class:

For purgatory groups:

1. **Passthrough transfer set**: exact-path roots for entries with mode `passthrough` (regular files, symlinks, empty directories). Transferred directly to final storage.
2. **Purgatory transfer set**: exact-path roots for ordinary postprocess entries plus subtree roots for postprocessed directories. Transferred to the staging area.

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
- Covered descendants are skipped in status and never cleaned locally.

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
* **Regular file**: committed via temp-file + atomic rename.
* **Symlink**: committed via temp symlink + atomic rename.
* **Unsupported or missing**: input entry fails.

`keep_original = true` commits the original work-area input entry/root as one output. For directories this commits the subtree recursively.

After collection, outputs are deduplicated while preserving order.

The status `postprocess` field is derived by matching rules against the logical normalized path (e.g., `videos/video.mp4`), not the absolute work-area path.

## Malformed status handling

If a `status.toml` exists but is malformed (invalid TOML or missing required fields), the status command returns a parse error rather than silently skipping it.

## Directory layout

```
<purgery_root>/                      # staging area
  <nickname>/
    incoming/<run_id>/               # client uploads here
      lease.toml
      run.toml
      manifest.toml
      files/                         # uploaded entries by sync mapping
    ready/<run_id>/                  # upload complete, pending processing
    processing/<run_id>/             # actively being processed
    done/<run_id>/                   # successfully processed
    failed/<run_id>/                 # failed runs
      status.toml
      run.toml
      manifest.toml
      ...

<root>/                              # final storage
  .purgery-work/<nickname>/<run_id>/ # work area (temporary)
  <nickname>/
    <sync.to>/                       # final imported entries
      ...
```

## Postprocessing conformance and import-and-retire

Postprocessing requires `delete_after_import = true`. This is an intentional conformance tradeoff, not an arbitrary safety constraint.

Purgery does not retain indefinite source-file metadata on the server. After a run completes, the server only keeps a bounded run status — it does not maintain a permanent record of every source file's identity, fingerprints, or processing history.

For passthrough imports, this is fine: the archive contains the same file content that came from the source tree. If the same import runs again, rsync converges the archive toward the source tree using ordinary file replacement.

For postprocessed imports, the archive does **not** contain the original source file. It contains transformed outputs: compressed videos, converted files, renamed outputs, or whatever the postprocess step produced. Purgery cannot use the archive alone to answer whether an unchanged local original:

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
4. The client deletes the unchanged local regular-file original after server-confirmed import.

The source original is removed from the source tree after successful import, so it will not be repeatedly reprocessed by later runs.

## Planned final-path validation

A run is rejected before entry processing if planned final paths conflict, including direct manifest-entry paths and postprocess-derived output roots.

## Cleanup authority by entry type

There are two distinct cleanup authorities:

1. **Server-confirmed cleanup** — applies to postprocess/transformed entries. Local deletion is authorized by a valid server status file.
2. **Transfer-confirmed cleanup** — applies to passthrough entries with `delete_after_import = true`. Local deletion is authorized by a durable local cleanup state file.

### Postprocess entries (server-confirmed cleanup)

For postprocess entries, cleanup authority is the server's `status.toml`:

1. The server's `status.toml` is valid and parseable.
2. `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`.
3. The file's own status is `imported` (covered/skipped entries are not deleted).
4. The manifest entry kind is `regular_file`.
5. The local file still matches the uploaded identity (size, mtime, and optional SHA-256) and is still a regular file (not a symlink replacement).
6. The sync mapping has `delete_after_import = true`.

### Passthrough entries (transfer-confirmed cleanup)

For passthrough entries, cleanup authority is the durable local cleanup state:

1. A valid cleanup state file exists on disk with a recorded rsync success marker (i.e. `rsync_succeeded = true`).
2. The cleanup identity was captured **before** rsync — either from a pre-rsync source walk (PassthroughDeleteAfterImport) or from the pre-rsync manifest (purgatory passthrough remainder).
3. The local file still matches the recorded identity (size, mtime, optional SHA-256).
4. The local file is still a regular file (not a symlink replacement).
5. The sync mapping has `delete_after_import = true`.

The cleanup state is always written before the passthrough rsync, with `rsync_succeeded = false`. Deletion is authorized only after rsync succeeds and the success marker is durably recorded.

Passthrough entries with `delete_after_import = false` are never cleaned.

### General properties

Deletion is idempotent. If the local file is already gone, it is counted as a successful cleanup. If the file changed since upload, the client leaves it untouched. Directories and symlinks are never deleted.
