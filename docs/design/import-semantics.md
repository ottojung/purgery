# Import Semantics

## Temp-file commit

Outputs are never copied directly to their final user-visible path. Each committed output goes through:

```
work output → final parent dir / .purgery-commit.<run_id>.<filename>.tmp → rename → final path
```

The temp file is on the same filesystem as the final path, so the rename is atomic against readers. Temp files are cleaned up after a successful commit.

## Conflict policy: atomic regular-file replacement

A missing final path is created, and an existing regular final file is atomically replaced. Existing directories, symlinks, parent-path symlinks, and other non-regular filesystem objects block the output and fail that file.

## Multi-output preflight and replay

Before committing any output for a file, all final output paths are derived and prechecked for root containment, symlinks, and replaceable destination types. Commits proceed in order. Already committed outputs are not removed or restored if a later commit fails. If processing stops before status publication, `process-once` replays the run from staged files and converges through atomic replacement.

See [Crash Safety and Idempotent Imports](crash-safety-and-idempotence.md) for the durable recovery and replacement invariants.

## `final_paths` (plural)

Status entries use `final_paths` — a list of all committed paths relative to the server root. A single-output import produces one entry. Postprocessing (e.g., `compress-video`) may produce multiple outputs (original + compressed).

For a failed file, `final_paths` is empty and the `error` field contains a description.

## Per-file errors

Per-file failures produce individual `FileStatusEntry` records with `status = "failed"` and a descriptive `error` field. The server continues processing remaining files. Only truly catastrophic errors (unreadable run config, invalid regex, missing step reference, unparseable manifest, envelope mismatch) abort the entire run.

## Work area

The server rebuilds a hidden work area for each processing attempt at `<root>/.purgery-work/<nickname>/<run_id>/`. Files are copied into subdirectories mirroring the destination structure: `<work_area>/<to_path>/<relative_path>`.

Cleanup policy:

| Run state | Work area kept? |
|-----------|-----------------|
| `done`    | removed         |
| `partial` | kept            |
| `failed`  | kept            |

## Run plan validation

Before processing any files, the server builds a `RunPlan` that compiles all postprocess regexes once and resolves every referenced step against the server config. If any regex is invalid or any step is missing on the server, the run is rejected with a run-level `Failed` status before any file is imported. File processing uses the precompiled plan and never recompiles regexes.

## Postprocess outputs

Expected outputs must be plain file-name patterns (no paths, no directories). Only `{file_name}`, `{file_stem}`, and `{stem}` placeholders are allowed; `{input}` and `{parent}` are forbidden in expected outputs (they remain allowed in `args`).

Expected outputs are verified to exist and to resolve inside the work area (within the same parent directory as the input). After collection, outputs are deduplicated while preserving order.

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
      files/                         # uploaded files by sync mapping
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
    <sync.to>/                       # final imported files
      ...
```

## Status envelope verification

Before deleting any local file, the client verifies:

- `status.nickname == manifest.nickname`
- `status.run_id == manifest.run_id`

If either mismatches, cleanup is aborted and nothing is deleted.

## Client deletion semantics

Local files are deleted only after all of the following are true:

1. The server's `status.toml` is valid and parseable.
2. `status.nickname == manifest.nickname` and `status.run_id == manifest.run_id`.
3. The file's status is `imported`.
4. The local file still matches the uploaded identity (size, mtime, and optional SHA-256).
5. The sync mapping has `delete_after_import = true`.

Deletion is idempotent. If the local file is already gone, it is counted as a successful cleanup. If the file changed since upload, the client leaves it untouched.
