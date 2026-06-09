# Purgery Configuration

## Server config

The server reads a TOML configuration from one of these locations (checked in order):

1. `--config PATH` (explicit)
2. `$PURGERY_CONFIG` environment variable
3. `~/.config/purgery/server.toml`
4. `/etc/purgery/server.toml`

Example:

```toml
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"

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
| `root` | yes | Absolute path to final storage root |
| `purgery_root` | yes | Absolute path to staging area for incoming uploads |
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

## Client config

Example:

```toml
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
from = "/home/user/Pictures"
to = "pictures"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `nickname` | yes | Machine identifier (alphanumeric, hyphens, underscores) |
| `server` | yes | Server connection details |
| `sync` | `[]` | List of sync mappings |
| `postprocess` | | Postprocess rule configuration |

### Server connection

| Field | Default | Description |
|-------|---------|-------------|
| `host` | — | SSH hostname |
| `command` | `"purgery-server"` | Remote server command prefix |

### Sync mapping

| Field | Default | Description |
|-------|---------|-------------|
| `name` | — | Unique sync name |
| `from` | — | Local source path |
| `to` | — | Relative destination under `root / nickname` |
| `delete_after_import` | `false` | Delete unchanged local regular files after confirmed import; directories and symlinks remain |

### Postprocess rule

| Field | Required | Description |
|-------|----------|-------------|
| `match` | yes | Rsync include/exclude pattern matching normalized import paths (rsync syntax, not regex) |
| `steps` | yes | List of server-defined step names to apply |

### Match patterns and import modes

Each entry is classified as **passthrough** or **postprocessed** (purgatory).

- Passthrough entries are transferred directly to final server storage by a bulk rsync call. They have no server bookkeeping: no manifest entry, no receipt, no status entry.
- Postprocessed entries are transferred to the server's staging area, where subprocesses run before final commit. They are tracked in the server manifest and status.

The `match` value is an rsync include/exclude pattern. Rules are evaluated in order. The first matching rule selects the entry for postprocessing with that rule's steps. If no rule matches, the entry is passthrough.

Supported rsync pattern syntax:

| Syntax | Meaning |
|--------|---------|
| `*` | Matches any characters except `/` |
| `**` | Matches any characters including `/` |
| `?` | Matches any single character except `/` |
| Leading `/` | Anchors the pattern to the start of the path |
| No leading `/` | Pattern matches at any position in the path |

Patterns are evaluated relative to each sync source root (`sync.from`), not the server destination. So for:

```toml
[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = "**/*.mp4"
```

the pattern sees `a.mp4`, `subdir/b.mp4` — not `videos/a.mp4`.

Examples:

```
match = "*.mp4"                    # any .mp4 file in any directory
match = "videos/*"                 # files directly inside videos/ subdirectory
match = "videos/**/*.mp4"          # any .mp4 under videos/ and its subdirectories
match = "/photos"                  # exactly the photos directory at source root
```

### Transfer roots

Each transfer set contains either **exact path roots** (regular files, symlinks, empty directories — each transferred as one independent entry) or **subtree path roots** (postprocessed directories whose entire subtree is transferred as a unit).

A postprocessed directory root generates an rsync filter that includes the directory and all its descendants (`dir/**`). Covered descendants are excluded from independent transfer roots because they are already included under the postprocessed directory subtree root.

### Empty transfer sets

If a sync group has no passthrough transfer roots, the passthrough rsync is skipped. If a sync group has no purgatory transfer roots, the purgatory rsync is skipped.

### Rsync filter generation

For each `[[sync]]` group, the client generates at most two rsync calls:

1. **Purgatory call**: transfers entries matching any `match` rule to the server's staging area.
2. **Passthrough call**: transfers all other entries directly to final storage.

Both calls use `rsync --archive --no-inc-recursive --protect-args --no-delete` with include/exclude filters.

The purgatory filter includes ancestor traversal directories needed to reach selected roots, then the roots themselves (exact or subtree), then excludes everything else.

### Import modes and cleanup

- **Passthrough regular files with `delete_after_import = false`**: not cleaned locally. Successful rsync is the import.
- **Passthrough regular files with `delete_after_import = true`**: cleaned locally after durable disk-backed state atomically records rsync success. Local identity (size, mtime, optional SHA-256) is verified before deletion.
- **Postprocessed regular files**: deleted after server status confirms the entry as imported.
- **Directories and symlinks**: never deleted regardless of mode.

## Run config

The uploaded run configuration (`run.toml`) is a subset of the client config. It includes `nickname`, sync mappings (name + `to` path only), and postprocess rules. It does **not** include server host/command, `purgery_root`, or local source `from` paths. This keeps server topology server-owned.

```toml
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
```

## Config strictness

All config structs reject unknown fields. Old configs with stale or misspelled fields produce clear errors rather than being silently ignored.
