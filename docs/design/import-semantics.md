# Import semantics

Purgery operates on one source entry per `sync` invocation.

## Direct passthrough

Without `--transform`, the client runs rsync directly to `USER@HOST:DESTINATION`. It does not create a server run, upload a manifest, poll status, or invoke `finish-run`.

The source entry is transferred with trailing slashes stripped. Trailing slashes on the source operand do not change source-entry semantics. Destination placement follows rsync: a file uses an existing directory or a slash-terminated operand as its parent, while a missing non-slash operand is its exact new name. An empty directory can likewise be renamed; a recursively transferred non-empty directory is placed beneath a missing destination directory.

Directory-forcing syntax on `DESTINATION` is retained in run and recovery data. A trailing slash, terminal `/.`, `.`, and `./` all express directory intent. Transform runs classify the staged source and destination once, atomically persist the resulting exact-target or directory-target plan, and reuse that plan on retries.

With `--delete-after-import`, the client first records durable local identity. Successful rsync authorizes cleanup, but the local entry is removed only if its current kind and identity still match the recorded entry.

## Transform

With `--transform`, `--delete-after-import` is required. The client creates an incoming run, writes `run.toml` and `manifest.toml`, and calls `prepare-run`. Plan validation checks the manifest envelope and the requested transform before any staged transfer and before `finish-run` can move the run to `ready`.

The source entry is staged at `files/<source-name>`. The server verifies the staged kind and identity before processing. The transform work area lives under the processing run in `work_dir`. The subprocess receives the staged source path as `{input}`.

For a directory source, the staging tree is the directory itself. The subprocess receives the directory path and may read its contents. The manifest describes one logical entry regardless of directory depth.

The run destination operand identifies final storage using rsync classification. `{target_directory}` is the parent of the resolved target path.

`expected_outputs` are path patterns with placeholders. After placeholder expansion:

- If the expanded path is absolute, it is used as-is.
- If the expanded path is relative, it is resolved against the persisted `{target_directory}`.
- `{target_directory}` is allowed and expands to the entry's target parent path.

### Transform finalization contract

Transforms are trusted final writers.

For transform runs, Purgery does not move or commit transform outputs. The configured transform program is responsible for writing its outputs directly to the final paths implied by `expected_outputs`. The transform program runs with the server process permissions and is trusted server-admin configuration. Purgery does not sandbox transform output paths — it only verifies and reports the paths configured by the server admin.

That means the transform program must:

- create parent directories when needed;
- avoid leaving bad partial outputs, or implement its own temporary-file-and-rename discipline;
- decide what to do if an output path already exists;
- preserve whatever permissions, timestamps, or metadata it cares about.

After the subprocess exits successfully, Purgery resolves `expected_outputs` and checks that each declared output path exists and is a supported filesystem entry. That check is not an atomic publication mechanism.

`expected_outputs = []` is valid. In that case, successful subprocess exit is sufficient for the entry to be marked imported, and `final_paths` is empty. This supports verification-only or deletion-only transforms.

Purgery does not stage, rename, move, or commit transform outputs. Local cleanup after a successful transform run is authorised by terminal server status reporting imported entries — not by the presence of final output files.

Transformed inputs are consumed by the transform flow and are never placed as final outputs. A transform produces exactly the declared `expected_outputs`. If `expected_outputs = []`, no output-existence checks are performed; successful subprocess exit is sufficient for the entry to be marked `imported`. This allows intentionally deletion-only or verification-only transforms.

Examples:

- `expected_outputs = ["{target_directory}/{file_stem}.Z.webm"]` with source `video.mp4` → `final_paths` records `/archive/video.Z.webm`.
- `expected_outputs = ["{target_directory}/{file_stem}.Z.webm"]` with source `Videos/2024/a.mp4` → `final_paths` records `/archive/2024/a.Z.webm`.
- The short relative form `expected_outputs = ["{file_stem}.Z.webm"]` is also valid and resolves against `<DESTINATION>`.

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

## Destination effects

Direct passthrough imports (rsync) overlay the destination tree without deleting absent destination entries. Directories merge, regular files replace conflicting files or empty directories, and symlinks are placed as symlinks with literal targets.

Transform entries instead rely on the transform program to write outputs directly to destination paths. Purgery checks declared `expected_outputs` after subprocess exit and records their paths in status. Relative `expected_outputs` resolve against the persisted target directory; absolute `expected_outputs` are used as-is. `expected_outputs = []` is valid and records no final paths.
