# Purgery Specification

## 1. Overview

Purgery is a Rust service for syncing files from client machines to a server, optionally postprocessing selected files, moving them into final server-side storage, and then allowing the client to safely delete the original local files.

The core lifecycle is:

```text
client local files
  -> rsync upload to server staging area
  -> server validates uploaded run
  -> server postprocesses matching files
  -> server moves imported files into final storage
  -> server writes success status
  -> client deletes only confirmed local originals
```

There are two binaries:

```text
purgery-client
purgery-server
```

Both are written in Rust.

## 2. Trust and Authorization Model

Clients connect to the server over SSH.

The SSH account and filesystem permissions are the authorization mechanism. Clients are ultimately trusted users, but the software should still validate all config, manifests, paths, and status files for correctness.

Client-provided configuration may select predefined postprocessing steps, but must not define arbitrary commands.

## 3. Server Configuration

The server has a TOML config file.

Example:

```toml
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
state_dir = "/var/lib/purgery"
log_dir = "/var/log/purgery"

[postprocess]
max_parallel_jobs = 1

[postprocess.steps.compress-video]
kind = "builtin"
command = "my-compress-video"
```

### Fields

`root`

Absolute path under which final synced files are stored.

`purgery_root`

Absolute path under which clients upload temporary runs.

Default example:

```text
/universe/tmp/purgery
```

`state_dir`

Absolute path for internal server state.

The server may use this for locks, job state, and bookkeeping. A future implementation may use SQLite here.

`log_dir`

Absolute path for logs.

`postprocess.max_parallel_jobs`

Maximum number of postprocessing jobs the server may run at once.

`postprocess.steps`

Map of predefined postprocessing steps that client configs are allowed to reference.

Initially there is one step:

```text
compress-video
```

It runs:

```sh
my-compress-video --input "$path"
```

where `$path` is the final destination path of the matched file.

## 4. Client Configuration

Each client has a TOML config file.

Example:

```toml
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/universe/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/vitalik/Videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
from = "/home/vitalik/Pictures"
to = "pictures"
delete_after_import = true

[[postprocess.rules]]
match = '^videos/.*\.(mp4|mov|mkv|webm)$'
steps = ["compress-video"]
```

### Fields

`nickname`

A short name identifying the client machine or sync source.

The nickname is used as a namespace on the server.

Final files are stored under:

```text
server.root / nickname / sync.to / relative_path
```

`server.host`

SSH host used by `purgery-client`.

`server.purgery_root`

Server-side staging root.

`sync.name`

Unique name for a sync mapping within this client config.

`sync.from`

Absolute local path to sync from.

`sync.to`

Server-relative destination path under:

```text
server.root / nickname
```

`sync.delete_after_import`

If true, the client deletes a local file only after the server status file confirms that exact uploaded version was imported successfully.

`postprocess.rules.match`

Python-style regular expression syntax, implemented with the Rust `regex` crate where compatible.

Rules match normalized server-relative paths of the form:

```text
sync.to / relative_path
```

Paths use `/` as the separator.

`postprocess.rules.steps`

List of predefined postprocessing step names.

For now, the only required step is:

```text
compress-video
```

## 5. Server Directory Layout

The server staging area is organized by nickname and run.

```text
/universe/tmp/purgery/
  laptop/
    incoming/
      RUN_ID/
    ready/
      RUN_ID/
    processing/
      RUN_ID/
    done/
      RUN_ID/
    failed/
      RUN_ID/
```

A run directory has this structure:

```text
RUN_ID/
  config.toml
  manifest.toml
  files/
    ...
  status.toml
```

`config.toml`

The client config used for this run.

`manifest.toml`

A list of uploaded files and their local metadata.

`files/`

The files uploaded by rsync.

`status.toml`

Written by the server after processing.

The client initially uploads into:

```text
purgery_root / nickname / incoming / run_id
```

After rsync succeeds, the client atomically renames the run directory to:

```text
purgery_root / nickname / ready / run_id
```

The server only processes runs in `ready`.

To claim a run, the server atomically renames it from `ready` to `processing`.

## 6. Run IDs

Each client run has a unique `run_id`.

Recommended format:

```text
YYYY-MM-DDTHH-MM-SSZ-randomsuffix
```

Example:

```text
2026-06-08T18-45-12Z-9f03
```

The run ID must be unique within the client nickname.

## 7. Rsync Semantics

The client uploads files using `rsync` over SSH.

Required flags:

```sh
rsync --recursive --partial --archive ...
```

The client must not pass `--delete`.

The client uploads into the run’s `files/` directory, never directly into final storage.

The server is responsible for deleting files from purgery after they are successfully moved/imported.

## 8. Manifest Format

The client writes a `manifest.toml` for every run.

Example:

```toml
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
sha256 = "optional-but-recommended"
```

### File Identity

For deletion safety, each file entry should identify the exact local version that was uploaded.

Minimum identity:

```text
local_path
size
mtime_ns
```

Preferred identity:

```text
local_path
size
mtime_ns
sha256
```

Before deleting a local file, the client must check that the current local file still matches the uploaded identity. If the file has changed since upload, the client must not delete it.

## 9. Status Format

The server writes `status.toml` after processing a run.

The status file must be written atomically:

```text
status.toml.tmp -> status.toml
```

Example:

```toml
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"
state = "done"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_path = "laptop/videos/a.mp4"
postprocess = ["compress-video"]

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/b.mp4"
relative_path = "b.mp4"
status = "failed"
error = "compress-video failed"
```

Allowed file statuses:

```text
imported
failed
skipped
```

The client may delete local originals only for files with:

```text
status = "imported"
```

The client must not delete files with `failed`, `skipped`, missing status, malformed status, or missing run status.

## 10. Postprocessing Semantics

Postprocessing happens after the server notices a completed run in purgery and before the file is considered imported.

The server matches each uploaded regular file against the client’s postprocessing rules.

Rules match the normalized path:

```text
sync.to / relative_path
```

Example:

```text
videos/a.mp4
```

If a file matches a rule with `steps = ["compress-video"]`, the server moves or copies the uploaded file into its final destination area and then runs:

```sh
my-compress-video --input "$final_path"
```

The initial implementation should keep the original file by default.

So after compressing:

```text
video.mp4
video.Z.webm
```

both may exist.

The server marks the original file as `imported` only if the final destination file exists and all required postprocessing steps completed successfully.

A future config option may allow deleting originals after successful compression, but the initial implementation should prefer safety.

## 11. Moving Files to Final Storage

Final paths are computed as:

```text
server.root / nickname / sync.to / relative_path
```

The server must reject final paths that escape `server.root`.

The implementation must reject or safely normalize:

```text
absolute destination paths
paths containing ..
symlinks that escape the root
empty destination components
```

A robust import should use temporary final paths.

Example:

```text
/universe/synced/laptop/videos/.purgery-importing.a.mp4.tmp
```

Then atomically rename into place:

```text
/universe/synced/laptop/videos/a.mp4
```

If purgery and final storage are on different filesystems, a simple rename may fail. The server must handle this by copying to a final temporary path and then renaming within the final filesystem.

## 12. Client Deletion Semantics

The client deletes local files only after all of the following are true:

1. The client can read a valid `status.toml` for the run.
2. The run status is complete.
3. The file has `status = "imported"`.
4. The local file still has the same identity as the manifest entry.
5. The sync mapping has `delete_after_import = true`.

Deletion must be idempotent.

If a local file is already gone, the client may treat that as a successful local cleanup.

If a local file changed after upload, the client must leave it untouched.

## 13. Failure and Retry Semantics

A failed file should not block the whole run from producing useful statuses for other files.

If postprocessing fails for one file, that file gets:

```toml
status = "failed"
```

with an error message.

The server should leave enough information in logs and/or state to diagnose the failure.

The initial implementation may support manual retry by moving a failed run or file back into `ready`.

Automatic retries are optional for the first version.

## 14. Concurrency

The initial implementation may be single-process and conservative.

Server-side concurrency rules:

1. A run is claimed by atomic rename from `ready` to `processing`.
2. Only one server process may process a run.
3. The server should process files sequentially initially.
4. `postprocess.max_parallel_jobs` may be implemented later.

Client-side concurrency rules:

1. A client should not reuse a `run_id`.
2. Multiple runs from the same nickname may exist.
3. The server processes complete runs independently.

## 15. Rust Design Requirements

The project should use Rust.

Important types should encode invariants explicitly.

Recommended newtypes:

```text
Nickname
RunId
ServerRoot
PurgeryRoot
RelativeDestinationPath
NormalizedRelativePath
ManifestFileIdentity
ReadyRun
ProcessingRun
ImportedFile
```

Boundary data from TOML, manifests, status files, filesystem paths, and command-line arguments should be parsed into validated internal types before use.

Prefer explicit enums over booleans for state.

Example:

```rust
enum FileImportStatus {
    Imported(ImportedFile),
    Failed(FileImportError),
    Skipped(SkipReason),
}
```

Use `Result<T, E>` with specific error types.

Avoid passing raw strings or raw paths deep into the system when a validated newtype would capture a useful invariant.

## 16. CLI Shape

Suggested commands:

```sh
purgery-client sync --config ./client.toml
purgery-client cleanup --config ./client.toml --run-id RUN_ID
purgery-client sync-and-cleanup --config ./client.toml

purgery-server process-once --config ./server.toml
purgery-server daemon --config ./server.toml
```

`process-once`

Scan ready runs, process available work, then exit.

`daemon`

Continuously scan or watch for ready runs.

The first implementation may provide only:

```sh
purgery-client sync-and-cleanup --config ./client.toml
purgery-server process-once --config ./server.toml
```

## 17. Non-goals for the First Version

The first version does not need:

```text
network daemon protocol
HTTP API
multi-user authorization beyond SSH/filesystem permissions
arbitrary client-defined shell commands
server-side deletion requested by clients
bidirectional sync
rsync --delete
automatic conflict resolution outside nickname namespacing
distributed locking beyond atomic filesystem renames
```

## 18. Acceptance Criteria for Initial Repository

The initial repository should include:

```text
Cargo workspace
purgery-client binary
purgery-server binary
shared library crate for config/manifest/status/path types
example client config
example server config
README
AGENTS.md
GitHub Actions CI
Dependabot config
basic unit tests for config parsing and path validation
basic integration-style tests for manifest/status lifecycle without real SSH
```

The first useful milestone is:

```text
purgery-client can create a run directory layout locally or over SSH,
purgery-server process-once can read a ready run,
validate paths,
move files into final storage,
optionally run compress-video,
write status.toml,
and purgery-client can delete only confirmed unchanged local files.
```
