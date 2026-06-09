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

Per-entry failures produce individual `EntryStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining entries. Only truly catastrophic errors (unreadable run config, invalid regex, missing step reference, unparseable manifest, envelope mismatch) abort the entire run.

## Work area

The server creates a hidden work area at `<root>/.purgery-work/<nickname>/<run_id>/`. Entries are placed into the work area before processing.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

## Run plan validation

Before processing any entries, the server builds a `RunPlan` that compiles all postprocess regexes once, resolves every referenced step against the server config, and validates expected-output patterns. If any regex is invalid, any step is missing on the server, or any expected-output pattern is malformed, the run is rejected with a run-level `Failed` status before any entry is imported. Entry processing uses the precompiled plan and never recompiles regexes.

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

## Planned final-path validation

A run is rejected before entry processing if planned final paths conflict, including direct manifest-entry paths and postprocess-derived output roots.

## Status envelope verification

Before deleting any local file, the client verifies:

- `status.nickname == manifest.nickname`
- `status.run_id == manifest.run_id`

If either mismatches, cleanup is aborted and nothing is deleted.

## Client deletion semantics

Local files are deleted only after all of the following are true:

1. The server's `status.toml` is valid and parseable.
2. `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`.
3. The file's own status is `imported` (covered/skipped entries are not deleted).
4. The manifest entry kind is `regular_file`.
5. The local file still matches the uploaded identity (size, mtime, and optional SHA-256) and is still a regular file (not a symlink replacement).
6. The sync mapping has `delete_after_import = true`.

Deletion is idempotent. If the local file is already gone, it is counted as a successful cleanup. If the file changed since upload, the client leaves it untouched. Directories and symlinks are never deleted.
