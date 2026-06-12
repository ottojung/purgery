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

[postprocess]

[postprocess.steps.compress-video]
kind = "subprocess"
program = "/usr/local/bin/compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.Z.webm"]
keep_original = true
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `work_dir` | yes | Absolute path to Purgery's working/state directory. Contains all non-final server state: incoming runs, ready/processing/done/failed runs, lease files, manifests, status files, postprocess work areas, and temporary files used for atomic writes |
| `gc` | no | GC configuration (see below) |
| `postprocess` | no | Postprocessing configuration (see below) |

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

### Postprocess config

| Field | Default | Description |
|-------|---------|-------------|
| `steps` | `{}` | Map of named postprocess step definitions |

## Postprocess step definition

| Field | Required | Description |
|-------|----------|-------------|
| `kind` | yes | Must be `"subprocess"` |
| `program` | yes | Executable path or name (resolved via `PATH`) |
| `args` | `[]` | Arguments with placeholders |
| `expected_outputs` | `[]` | Output entry-root name patterns with placeholders |
| `keep_original` | `true` | Whether to keep the original input entry/root as one committed output |

### Placeholders

| Placeholder | Resolves to |
|-------------|-------------|
| `{input}` | Absolute work-area input path |
| `{parent}` | Work-area parent directory |
| `{file_name}` | Input file name with extension |
| `{file_stem}` | Input file name without extension |
| `{stem}` | Deprecated alias for `{file_stem}` |

`args` may use `{input}`, `{parent}`, `{file_name}`, `{file_stem}`, and `{stem}`. `expected_outputs` may use only `{file_name}`, `{file_stem}`, and `{stem}`, and each resolved expected output must be a plain file name without directory components.

A subprocess step must produce at least one committed output. If `keep_original = false`, then `expected_outputs` must be non-empty. This is validated at server boot time.

### Subprocess safety

Postprocess commands are always represented as argv-style argument vectors (the `args` list), never as shell strings. There is no shell interpolation, no `sh -c` invocation, and no concatenation of user-derived paths into shell snippets.

Placeholder expansion substitutes resolved paths into the argument vector directly as separate argv elements.

When invoking tools that parse their own options (e.g., ffmpeg, ImageMagick), filenames beginning with `-` may be misinterpreted as option flags by the subprocess. Recommended mitigations:

- Use `--` before the filename argument if the tool supports it (e.g., `args = ["--input", "--", "{input}"]`).
- Prefix the filename with `./` when the tool accepts relative paths.

Filenames containing spaces, newlines, or non-ASCII characters are handled correctly because argv-style invocation passes each argument as a separate C string without shell word splitting.

## Run config (run.toml)

The client uploads a run config to the server:

```toml
nickname = "laptop"
to = "user@example.com:/universe/synced/videos"
delete_after_import = true
```

The `to` field is the destination path as specified by the client. It may be absolute or relative. The server uses it to compute final archive paths: `{to}/{relative_entry_path}`.

## Config strictness

All config structs reject unknown fields. Old configs with stale or misspelled fields produce clear errors rather than being silently ignored.

## Cleanup identity requirements

Regular files that can authorize local deletion must have SHA-256 identity.

| Entry kind | Identity fields | SHA required? |
|------------|----------------|---------------|
| Regular file | size, mtime, SHA-256 | yes, for delete-authorizing entries |
| Symlink | literal link target | no |
| Directory | bottom-up subtree identity | no |

Cleanup behaviour by kind:

- **Regular file with SHA**: deletion is authorised when size, mtime, and SHA-256 all match captured identity. If SHA recomputation fails during verification, deletion is refused.
- **Regular file without SHA**: deletion is never authorised. The entry is excluded from cleanup ledgers.
- **Symlink**: deletion is authorised when the literal link target matches. The symlink is unlinked without following the target.
- **Directory**: tracked descendants must still match their captured identities.

### SHA computation failure during classification

- Postprocess regular files: SHA failure is fatal during manifest building.
- Passthrough regular files with `--delete-after-import`: SHA failure is fatal during manifest building when the entry may authorise cleanup.
- Pure passthrough pre-rsync cleanup ledger capture: a regular file whose SHA cannot be computed is skipped (not added to the ledger).

## Trust boundary assumptions

Purgery's security model assumes:

1. **Server config** is trusted server admin configuration.
2. **`--server-command`** is a trusted configuration value (not user input).
3. **Postprocess programs and their argv** are trusted server-side configuration set by the server admin. Clients request steps by name but never upload arbitrary commands.
4. **Source filenames and paths** may contain arbitrary characters (spaces, special characters, leading `-`) and must not become shell syntax or local subprocess options. Purgery uses argv-style invocation and shell-escaping for remote commands to prevent injection.
5. **Local filesystem and server storage** are non-hostile unless documented otherwise.

## Purgery-owned subprocess argv hardening

Purgery hardens its own `ssh` and `rsync` argv to prevent option injection:

| Subprocess | Pattern | Protection |
|------------|---------|------------|
| `ssh` | `ssh -- HOST COMMAND` | Prevents host value from being interpreted as an `ssh` option |
| `rsync` | `rsync [options...] -- SOURCE DEST` | Prevents source/destination paths from being interpreted as `rsync` options |

All `rsync` options, including filter merge files, appear before the `--` separator. Source and destination operands appear after it.

### Postprocess commands

Postprocess argv is trusted server-side configuration and is not rewritten, validated, or auto-fixed by Purgery. Purgery passes the configured `args` vector directly to the subprocess without modification.

## Server work area layout

Server postprocess work areas live under `work_dir`:

```text
{work_dir}/{nickname}/processing/{run_id}/work/
```

This is inside the run's processing directory. On successful completion (`Done`), the work area is removed before the run moves to `done`. On failure (`Failed` or `Partial`), the work area stays with the run directory as it moves to `failed` or `done`.
