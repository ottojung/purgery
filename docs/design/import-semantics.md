# Import semantics

Purgery operates on one source entry per `sync` invocation.

## Direct passthrough

Without `--postprocess`, the client runs rsync directly to `USER@HOST:DESTINATION`. It does not create a server run, upload a manifest, poll status, or invoke `finish-run`.

The source entry is transferred with trailing slashes stripped. Trailing slashes on the source operand do not change source-entry semantics. The final path is `<DESTINATION>/<SOURCE-NAME>`.

With `--delete-after-import`, the client first records durable local identity. Successful rsync authorizes cleanup, but the local entry is removed only if its current kind and identity still match the recorded entry.

## Postprocess

With `--postprocess`, `--delete-after-import` is required. The client creates an incoming run, writes `run.toml` and `manifest.toml`, and calls `prepare-run`. Plan validation checks the manifest envelope and every requested postprocess step before any staged transfer and before `finish-run` can move the run to `ready`.

The source entry is staged at `files/<source-name>`. The server verifies the staged kind and identity before processing. The postprocess work area lives under the processing run in `work_dir`. The subprocess receives the staged source path as `{input}`.

For a directory source, the staging tree is the directory itself. The subprocess receives the directory path and may read its contents. The manifest describes one logical entry regardless of directory depth.

The run destination is final storage. Final path computation:

- The source entry base final path is `<destination>/<source_entry_name>`.
- If `keep_original = true`, the original work entry commits to that base final path.
- Each expected postprocess output commits under the same parent as the base final path, using the output file name.

Examples:

- source `video.mp4` → `keep_original = true`: original commits to `/archive/video.mp4`, output `video.Z.webm` commits to `/archive/video.Z.webm`.
- source `Videos/2024/a.mp4` → `keep_original = true`: original commits to `/archive/2024/a.mp4`, output `a.Z.webm` commits to `/archive/2024/a.Z.webm`.

`work_dir` contains only Purgery state and work areas. It is never a final-storage root.

After processing, the client waits for a terminal state and reads a valid status envelope. Only entries marked `imported` authorize local cleanup. Failed and skipped entries remain local.

## Split

With `--split <PATTERN>`, the client discovers matching source entries under `<SOURCE>` and processes each as a separate operation. `<SOURCE>` itself is matched as the relative sentinel `"."`. Pure passthrough split performs one transfer of the selected roots. Cleanup and postprocess splits run serially.

## Destination overlay

Imports overlay the destination tree without deleting absent destination entries. Directories merge, regular files replace conflicting files or empty directories, and symlinks are committed as symlinks with literal targets.
