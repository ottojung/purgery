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

[transform]

[transform.steps.compress-video]
kind = "subprocess"
program = "/usr/local/bin/compress-video"
args = ["--input", "{input}", "--output-dir", "{target_directory}"]
expected_outputs = ["{file_stem}.Z.webm"]
keep_original = true
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `work_dir` | yes | Absolute path to Purgery's working/state directory. Contains all non-final server state: incoming runs, ready/processing/done/failed runs, lease files, manifests, status files, transform work areas, and temporary files used for atomic writes |
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
| `incoming_lease_secs` | `1800` | Lease duration for incoming runs |
| `heartbeat_interval_secs` | `60` | Recommended heartbeat interval |

### Transform config

| Field | Default | Description |
|-------|---------|-------------|
| `steps` | `{}` | Map of named transform step definitions |

## Transform step definition

| Field | Required | Description |
|-------|----------|-------------|
| `kind` | yes | Must be `"subprocess"` |
| `program` | yes | Executable path or name (resolved via `PATH`) |
| `args` | `[]` | Arguments with placeholders |
| `expected_outputs` | yes | List of output entry-name patterns with placeholders. May be empty (`[]`) |
| `keep_original` | `true` | Metadata for the transform program; Purgery does not use it to place or move files |

### Placeholders

| Placeholder | Resolves to |
|-------------|-------------|
| `{input}` | Absolute work-area input path |
| `{parent}` | Work-area parent directory |
| `{file_name}` | Input file name with extension |
| `{file_stem}` | Input file name without extension |
| `{stem}` | Deprecated alias for `{file_stem}` |
| `{target_directory}` | Directory where this entry's non-transform final path would be placed |

`args` may use `{input}`, `{parent}`, `{file_name}`, `{file_stem}`, `{stem}`, and `{target_directory}`. `expected_outputs` may use only `{file_name}`, `{file_stem}`, and `{stem}`, and each resolved expected output must be a plain file name without directory components.

Purgery does not move or commit transform outputs. The transform program is responsible for placing outputs into `{target_directory}`. After each step exits successfully, Purgery checks each declared expected output exists in `{target_directory}`. If `expected_outputs = []`, no output-existence checks are performed; successful subprocess exit is sufficient.

### Subprocess safety

Transform commands are always represented as argv-style argument vectors (the `args` list), never as shell strings. There is no shell interpolation, no `sh -c` invocation, and no concatenation of user-derived paths into shell snippets.

Placeholder expansion substitutes resolved paths into the argument vector directly as separate argv elements.

When invoking tools that parse their own options (e.g., ffmpeg, ImageMagick), filenames beginning with `-` may be misinterpreted as option flags by the subprocess. Recommended mitigations:

- Use `--` before the filename argument if the tool supports it (e.g., `args = ["--input", "--", "{input}"]`).
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

- The source entry base final path is `<destination>/<source_entry_name>`.
- `{target_directory}` is the parent directory of the base final path: `<destination>`.
- For non-transform entries, the work entry is committed to the base final path.
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

## Trust boundary assumptions

Purgery's security model assumes:

1. **Server config** is trusted server admin configuration.
2. **`--server-command`** is a trusted configuration value (not user input).
3. **Transform programs and their argv** are trusted server-side configuration set by the server admin. Clients request steps by name but never upload arbitrary commands. Transform programs are trusted to place outputs in `{target_directory}` and may intentionally perform deletion-only/no-output imports when `expected_outputs = []`.
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
