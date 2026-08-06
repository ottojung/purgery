# Purgery Configuration

## Config file discovery

Purgery supports config file discovery so you don't always need `--config`.

### Server config lookup

The server searches for its config in this order:

1. `--config PATH` (explicit CLI argument)
2. `$PURGERY_SERVER_CONFIG_PATH`, if set and non-empty
3. `$XDG_CONFIG_HOME/purgery/server.toml`, only when `XDG_CONFIG_HOME` is set and non-empty
4. `$HOME/.config/purgery/server.toml`, only when `HOME` is set and non-empty
5. `/etc/purgery/server.toml`

### Client

The client has no config file. All options are CLI arguments. The `--state-dir` option (defaulting to `$XDG_STATE_HOME/purgery`) controls where client state is stored.

## Server config

```toml
work_dir = "/var/lib/purgery/work"

[gc]
incoming_lease_secs = 1800
heartbeat_interval_secs = 60
ready_retention_secs = 3600
processing_retention_secs = 3600
done_retention_secs = 3600
failed_retention_secs = 86400
orphan_retention_secs = 86400

[[transform]]
name = "compress-video"
kind = "subprocess"
program = "/usr/local/bin/compress-video"
args = ["--input", "{input}", "--output", "{target_directory}/{target_file_stem}.Z.webm"]
expected_outputs = ["{target_directory}/{target_file_stem}.Z.webm"]
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `work_dir` | yes | Path to Purgery's working/state directory. Contains all non-final server state: incoming runs, ready/processing/done/failed runs, lease files, manifests, status files, transform work areas, and temporary files used for atomic writes |
| `gc` | no | GC configuration (see below) |
| `transform` | no | Transforming configuration (see below) |

### Logging config

| Field | Default | Description |
|-------|---------|-------------|
| `level` | `"info"` | Log level: `error`, `warn`, `info`, `debug`, `trace` |
| `format` | `"pretty"` | Output format: `pretty`, `compact`, `json` |
| `color` | `"auto"` | Color mode: `auto`, `always`, `never` |

### GC config

| Field | Default | Description |
|-------|---------|-------------|
| `incoming_lease_secs` | `1800` | Lease duration for resumable `incoming` uploads |
| `heartbeat_interval_secs` | `60` | Recommended heartbeat interval |
| `ready_retention_secs` | `3600` | Maximum time queued `ready` work may remain before GC expires it |
| `processing_retention_secs` | `3600` | Maximum unlocked `processing` recovery window before GC expires it; locked expired processing is warned about and retained until the lock is gone |
| `done_retention_secs` | `3600` | Terminal observation window for successful `done` metadata |
| `failed_retention_secs` | `86400` | Debugging window for `failed` requests before deletion |
| `orphan_retention_secs` | `86400` | Grace window for malformed, incompatible, or unknown request state before deletion |


### Server work-state lifecycle

`work_dir` is temporary operational state, not final storage, an archive, or permanent history. Request directories move through `incoming`, `ready`, `processing`, and terminal `done` or `failed` phases. `incoming`, `ready`, and unlocked `processing` are resumable only inside their retention windows. `done` and `failed` are terminal and exist only so clients and operators can observe outcomes briefly.

GC owns expired server work state in every phase. Terminal retention (`done`, `failed`) is measured from `status.toml` mtime, providing a stable clock that is not extended by GC's own pruning actions. Successful `done` requests keep `status.toml`, `run.toml`, and `manifest.toml` for `done_retention_secs`, but GC and finalization remove staged/control material such as `files/`, `work/`, `lease.toml`, `progress.toml`, `processor.lock`, and `*.tmp`. Expired terminal directories are deleted entirely; pruning within a run does not refresh the retention clock. Failed requests may keep payload/work material for `failed_retention_secs` for inspection. Incompatible or protocol-mismatched leases are not trusted for expiry and are governed by orphan retention. Malformed, incompatible, or unknown request directories are retained only for `orphan_retention_secs`; GC logs that it cannot understand them and then removes them after the grace window. Invalid top-level nickname directories under `work_dir` are also governed by `orphan_retention_secs` and logged with a warning.

### Transform config

Transforms are defined as an array of tables using `[[transform]]`. Each named transform is a separate table entry.

## Transform definition

| Field | Required | Description |
|-------|----------|-------------|
| `kind` | yes | Must be `"subprocess"` |
| `program` | yes | Executable path or name (resolved via `PATH`) |
| `args` | `[]` | Arguments with placeholders |
| `expected_outputs` | yes | List of output entry-name patterns with placeholders. May be empty (`[]`) |

### Placeholders

| Placeholder | Resolves to |
|-------------|-------------|
| `{input}` | Absolute work-area input path |
| `{parent}` | Work-area parent directory |
| `{file_name}` | Input file name with extension |
| `{file_stem}` | Input file name without extension |
| `{target_path}` | Resolved target path after rsync destination classification |
| `{target_directory}` | Parent directory of the resolved target path |
| `{target_file_name}` | Resolved target file name |
| `{target_file_stem}` | Resolved target file name without its extension |

`args` may use every placeholder above. `expected_outputs` may use the source name placeholders and all target placeholders, but not `{input}` or `{parent}`. After placeholder expansion:

- If the expanded path is absolute, it is used as-is.
- If the expanded path is relative, it is resolved against `{target_directory}`. An exact file destination is therefore never used as a directory.

### Transform finalization contract

Transforms are trusted final writers.

For transform runs, Purgery does not move or commit transform outputs. The configured transform program is responsible for writing its outputs directly to the final paths implied by `expected_outputs`.

That means the transform program must:

- create parent directories when needed;
- avoid leaving bad partial outputs, or implement its own temporary-file-and-rename discipline;
- decide what to do if an output path already exists;
- preserve whatever permissions, timestamps, or metadata it cares about.

After the subprocess exits successfully, Purgery resolves `expected_outputs` and checks that each declared output path exists and is a supported filesystem entry. That check is not an atomic publication mechanism.

`expected_outputs = []` is valid. In that case, successful subprocess exit is sufficient for the entry to be marked imported, and `final_paths` is empty. This supports verification-only or deletion-only transforms.

The transform program runs with the server process permissions and is trusted server-admin configuration. Purgery does not sandbox transform output paths.

Purgery does not stage, rename, move, or commit transform outputs. Local cleanup after a successful transform run is authorised by terminal server status reporting imported entries — not by the presence of final output files.

### Subprocess safety

Transform commands are always represented as argv-style argument vectors (the `args` list), never as shell strings. There is no shell interpolation, no `sh -c` invocation, and no concatenation of user-derived paths into shell snippets.

Placeholder expansion substitutes resolved paths into the argument vector directly as separate argv elements.

When invoking tools that parse their own options (e.g., ffmpeg, ImageMagick), filenames beginning with `-` may be misinterpreted as option flags by the subprocess. Recommended mitigations:

- Use `--` before the filename argument if the tool supports it (e.g., `args = ["--verbose", "--", "{input}"]`).
- Prefix the filename with `./` when the tool accepts relative paths.

Filenames containing spaces, newlines, or non-ASCII characters are handled correctly because argv-style invocation passes each argument as a separate C string without shell word splitting.

## Run config (run.toml)

The client uploads a run config to the server:

```toml
nickname = "laptop"
destination = "/universe/synced/videos"
delete_after_import = true
```

The `destination` field is the client-supplied destination path. It may be absolute or relative. For transform runs, `prepare-run` resolves a relative destination against the server's current working directory and atomically rewrites `run.toml` with the absolute path.

Final path computation:

- The source entry base final path is the exact destination or a child of the destination, as recorded by the resolved destination plan.
- `{target_directory}` is the parent directory of that resolved target path.
- For transform entries, the transform program places outputs in `{target_directory}`. Purgery does not move or commit outputs.
- `work_dir` is never final storage.

## Config strictness

All config structs reject unknown fields. Misspelled fields produce clear errors rather than being silently ignored.

## Cleanup identity requirements

Cleanup captures identity for safe local deletion. For a single source entry, identity is captured as follows:

| Source kind | Identity fields |
|-------------|----------------|
| Regular file | size, mtime, SHA-256 |
| Symlink | literal link target |
| Directory | recursive descendant identities (see below) |

### Directory source identity

When the source is a directory, the client recursively captures identity for every descendant at cleanup snapshot time. This does not create manifest entries for descendants — the manifest describes one logical source entry. The descendant identities are used only for safe local deletion after server-confirmed import.

Each descendant's identity follows the same rules: regular files capture size, mtime, and SHA-256; symlinks capture the literal link target; descendant directories are removed bottom-up after their children are gone.

### Identity verification

- **Regular file with SHA**: deletion is authorised when size, mtime, and SHA-256 all match captured identity.
- **Regular file without SHA**: deletion is never authorised. The entry is excluded from cleanup ledgers.
- **Symlink**: deletion is authorised when the literal link target matches. The symlink is unlinked without following the target.
- **Descendant directory**: tracked children must still match their captured identities before the parent can be removed.
- **Untracked content**: an on-disk descendant not captured in the cleanup snapshot prevents its containing directory from being removed.

### SHA computation failure

- Transform source files: SHA failure is fatal during manifest building.
- Passthrough files with `--delete-after-import`: SHA failure is fatal during cleanup identity capture.
- Pure passthrough pre-rsync cleanup ledger capture: a regular file whose SHA cannot be computed is skipped (not added to the ledger).

## Client nickname

`purgery-client sync` accepts an optional `--nickname NAME` flag.

The nickname identifies the source/client namespace on the server:

```text
nickname    = source/client namespace
run_id      = unique operation identity
destination = final import target
```

The nickname is **not** the remote host. It is a stable local identifier so the
server can distinguish runs from different sources.

If `--nickname` is omitted, the client derives a nickname automatically from
the local username and hostname, for example:

```shell
USER=alice hostname=my-laptop  →  nickname=alice-my-laptop
```

No persistent client identity file is written.

Use `--nickname` when you want an explicit, stable identifier:

```shell
purgery-client sync --nickname framework-laptop -- ~/Videos user@server:/archive
```

Invalid nicknames (containing spaces, special characters, etc.) are rejected
with a clear error. See `Nickname` validation rules for the allowed character
set.

## Trust boundary assumptions

Purgery's security model assumes:

1. **Server config** is trusted server admin configuration.
2. **`--server-command`** is a trusted configuration value (not user input).
3. **Transform programs and their argv** are trusted server-side configuration set by the server admin. Clients request a transform by name but never upload arbitrary commands. Transform programs are trusted to place outputs at configured paths and may intentionally perform deletion-only/no-output imports when `expected_outputs = []`.
4. **Source filenames and paths** may contain arbitrary characters (spaces, special characters, leading `-`) and must not become shell syntax or local subprocess options. Purgery uses argv-style invocation and shell-escaping for remote commands to prevent injection.
5. **Local filesystem and server storage** are non-hostile unless documented otherwise.

## Purgery-owned subprocess argv hardening

Purgery hardens its own `ssh` and `rsync` argv to prevent option injection:

| Subprocess | Pattern | Protection |
|------------|---------|------------|
| `ssh` | `ssh -- HOST COMMAND` | Prevents host value from being interpreted as an `ssh` option |
| `rsync` | `rsync [options...] -- SOURCE DEST` | Prevents source/destination paths from being interpreted as `rsync` options |

All `rsync` options, including filter merge files, appear before the `--` separator. Source and destination operands appear after it.

### Transform commands

Transform argv is trusted server-side configuration and is not rewritten, validated, or auto-fixed by Purgery. Purgery passes the configured `args` vector directly to the subprocess without modification.

## Server work area layout

Server transform work areas live under `work_dir`:

```text
{work_dir}/{nickname}/processing/{run_id}/work/
```

This is inside the run's processing directory. On successful completion (`Done`), the work area is removed before the run moves to `done`. On failure (`Failed` or `Partial`), the work area stays with the run directory as it moves to `failed` or `done`.
