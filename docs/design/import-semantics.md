# Import semantics

Purgery has three invocation modes.

## Direct passthrough

Without `--postprocess`, the client runs rsync directly to `USER@HOST:DESTINATION/`. It does not create a server run, upload a manifest, poll status, or invoke `finish-run`.

With `--delete-after-import`, the client first records durable local identity. Successful rsync authorizes cleanup, but each local entry is removed only if its current kind and identity still match the recorded entry.

## Postprocess

With `--postprocess`, `--delete-after-import` is required. The client creates an incoming run, writes `run.toml` and `manifest.toml`, and calls `prepare-run`. Plan validation checks the manifest envelope and every requested postprocess step before any staged transfer and before `finish-run` can move the run to `ready`.

Files are staged at `files/<relative-path>`. The server verifies staged kinds, regular-file identity, and literal symlink targets before processing. Postprocess work areas live under the processing run in `work_dir`.

The run destination is final storage. For an entry `trip/a.mp4`:

- destination `/universe/synced/videos` commits to `/universe/synced/videos/trip/a.mp4`;
- destination `incoming/videos` commits to `incoming/videos/trip/a.mp4`, relative to the remote server process environment.

`work_dir` contains only Purgery state and work areas. It is never a final-storage root.

After processing, the client waits for a terminal state and reads a valid status envelope. Only entries marked `imported` authorize local cleanup. Failed and skipped entries remain local, and imported entries are still retained if their local identity changed.

## Destination overlay

Imports overlay the destination tree without deleting absent destination entries. Directories merge, regular files replace conflicting files or empty directories, and symlinks are committed as symlinks with literal targets. Destination parent components are checked so symlink directories are not followed during commit.
