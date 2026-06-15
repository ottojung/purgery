# Import semantics

Purgery operates on one source entry per `sync` invocation.

## Direct passthrough

Without `--transform`, the client runs rsync directly to `USER@HOST:DESTINATION`. It does not create a server run, upload a manifest, poll status, or invoke `finish-run`.

The source entry is transferred with trailing slashes stripped. Trailing slashes on the source operand do not change source-entry semantics. The source entry base final path is `<DESTINATION>/<SOURCE-NAME>`.

With `--delete-after-import`, the client first records durable local identity. Successful rsync authorizes cleanup, but the local entry is removed only if its current kind and identity still match the recorded entry.

## Transform

With `--transform`, `--delete-after-import` is required. The client creates an incoming run, writes `run.toml` and `manifest.toml`, and calls `prepare-run`. Plan validation checks the manifest envelope and every requested transform step before any staged transfer and before `finish-run` can move the run to `ready`.

The source entry is staged at `files/<source-name>`. The server verifies the staged kind and identity before processing. The transform work area lives under the processing run in `work_dir`. The subprocess receives the staged source path as `{input}`.

For a directory source, the staging tree is the directory itself. The subprocess receives the directory path and may read its contents. The manifest describes one logical entry regardless of directory depth.

The run destination is final storage. `{target_directory}` is `<destination>` (the parent of the base final path `<destination>/<source_entry_name>`).

Transform programs are responsible for placing outputs into `{target_directory}`. After each subprocess step exits successfully, Purgery checks that each declared expected output exists in `{target_directory}`. Purgery does not move or commit transform outputs.

If `expected_outputs = []`, no output-existence checks are performed. Successful subprocess exit is sufficient for the entry to be marked `imported`. This allows intentionally deletion-only or verification-only transforms.

Examples:

- source `video.mp4` → transform writes `{target_directory}/video.Z.webm`; `final_paths` records `/archive/video.Z.webm`.
- source `Videos/2024/a.mp4` → transform writes `{target_directory}/a.Z.webm`; `final_paths` records `/archive/2024/a.Z.webm`.

`work_dir` contains only Purgery state and work areas. It is never a final-storage root.

After processing, the client waits for a terminal state and reads a valid status envelope. Only entries marked `imported` authorize local cleanup. Failed and skipped entries remain local.

## Split

With `--split <PATTERN>`, Purgery processes matching source entries under `<SOURCE>`.

### Pure passthrough split

Without `--delete-after-import` and `--transform`, the client builds constant rsync filter rules from the pattern and runs one rsync filter transfer. No Purgery-side candidate discovery, server run, manifest, or cleanup state is created. The contract is final destination effect under the generated filter rules.

`--split "."` uses ordinary source-entry rsync. All other patterns use one rsync with `--include=*/`, `--include=<dir-payload>`, `--include=<entry>`, and `--exclude=*`. The transfer uses `--prune-empty-dirs`, so traversal-only empty directories are removed and selected empty directories may not be created at the destination.

### Cleanup/transform split

With `--delete-after-import` or `--transform`, the client discovers candidates using Purgery's own pattern matcher, ancestor-prunes matched roots (descendants of matched ancestors are not scheduled as separate operations, but their data remains in the ancestor payload), sorts deterministically, and runs each root as a serialized non-split sync operation. Each operation completes entirely before the next begins.

`<SOURCE>` itself is matched as the relative sentinel `"."`. Source trailing slashes, `.`, and `..` are normalized before split discovery.

## Destination overlay

Imports overlay the destination tree without deleting absent destination entries. Directories merge, regular files replace conflicting files or empty directories, and symlinks are committed as symlinks with literal targets.
