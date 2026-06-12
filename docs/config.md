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

### Client config lookup

The client searches for its config in this order:

1. `--config PATH` (explicit CLI argument)
2. `$PURGERY_CLIENT_CONFIG_PATH`, if set and non-empty
3. `$XDG_CONFIG_HOME/purgery/client.toml`, only when `XDG_CONFIG_HOME` is set and non-empty
4. `$HOME/.config/purgery/client.toml`, only when `HOME` is set and non-empty

Purgery does not invent `/root` or any other default when `HOME` is absent.

The client does not fall back to `/etc/purgery/client.toml` — client config is per-user only.

If no config is found, Purgery emits a clear error listing every path it checked.

## Operational vs config-file paths

**Config files** may be discovered from standard config locations (CLI, env var, XDG, HOME, /etc).

**All other operational files and directories** must be specified in the parsed config:

- Server: `root` (final archive) and `work_dir` (all working state)
- Client: `state_dir` (all local state, temp files, cleanup ledgers, rsync filters)

Purgery does not fall back to `$XDG_STATE_HOME`, `$HOME`, `/tmp`, `std::env::temp_dir()`, or any other implicit location for operational state. Every non-config filesystem path comes from a configured value.

## Server config

```toml
root = "/universe/synced"
work_dir = "/universe/tmp/purgery"

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
| `root` | yes | Absolute path to final storage root (archive destination) |
| `work_dir` | yes | Absolute path to Purgery's working/state directory. Contains all non-final server state: incoming runs, ready/processing/done/failed runs, lease files, manifests, status files, postprocess work areas, and temporary files used for atomic writes. No internal work directories are created under `root` |
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

Postprocess commands are always represented as argv-style argument vectors (the `args` list), never as shell strings. There is no shell interpolation, no `sh -c` invocation, and no concatenation of user-derived paths into shell snippets. This prevents shell injection from filenames or paths containing shell metacharacters.

Placeholder expansion substitutes resolved paths into the argument vector directly as separate argv elements. User-derived paths (source filenames, directory names) are never spliced into shell syntax.

When invoking tools that parse their own options (e.g., ffmpeg, ImageMagick), filenames beginning with `-` may be misinterpreted as option flags by the subprocess. Recommended mitigations:

- Use `--` before the filename argument if the tool supports it (e.g., `args = ["--input", "--", "{input}"]`).
- Prefix the filename with `./` when the tool accepts relative paths.
- Document any tool-specific safe argument placement in your step definitions.

Filenames containing spaces, newlines, or non-ASCII characters are handled correctly because argv-style invocation passes each argument as a separate C string without shell word splitting.

## Client config

Example:

```toml
nickname = "laptop"
state_dir = "/var/lib/purgery"

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
| `state_dir` | yes | Writable state directory for all client-owned operational state. Must be a non-empty absolute path. Contains cleanup ledgers, temporary filter files, and per-run temp subtrees |
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
| `delete_after_import` | `false` | Remove unchanged local originals after confirmed import (regular files by size/mtime/SHA, symlinks by link target, directories bottom-up) |

### Postprocess rule

| Field | Required | Description |
|-------|----------|-------------|
| `match` | yes | Rsync include/exclude pattern matching normalized import paths (rsync syntax, not regex) |
| `steps` | yes | List of server-defined step names to apply |
| `for` | no | Optional list of sync group names the rule applies to. Omitted means every sync group. Empty list is invalid. Unknown sync names are rejected at parse time |

### Rule scoping

A `for` field scopes a rule to specific sync groups:

```toml
# Applies to all sync groups (no for)
[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]

# Applies only to the "videos" sync group
[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
for = ["videos"]
```

Before a sync group is scanned, the client computes `applicable_rules(sync_name)`. A rule is applicable when its `for` is omitted or the sync group name is listed.

### Postprocessing requires delete_after_import (conformance tradeoff)

A sync group with applicable postprocess rules must set `delete_after_import = true`. This is not an arbitrary safety constraint — it follows from Purgery's import-and-retire model.

Why this rule exists: Purgery does not retain indefinite source-entry metadata on the server. Because transformed outputs are not the original source entries, the final archive alone cannot tell Purgery whether an unchanged local original has already been processed in a previous run. It cannot know:

* whether the original was already processed;
* whether it was processed with the same rule set and step definitions;
* whether it produced the same expected outputs;
* whether it should be skipped or reprocessed;
* or whether it represents a changed source that happens to map to the same archive destination.

Solving this would require persistent server-side source fingerprints, retained manifests, or an indefinitely growing receipt ledger — which Purgery explicitly avoids.

Postprocessing is therefore modeled as import-and-retire: the source entry is uploaded, transformed, and the confirmed local original is removed after server-confirmed import. This prevents repeated reprocessing of the same original on subsequent runs.

```toml
# This configuration is INVALID
[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
# for omitted — applies to all sync groups, including "videos"
```

This config is rejected because the rule applies to `videos` and `videos` has `delete_after_import = false`. The error message explains the conformance reason.

A sync group with no applicable postprocess rules is unaffected — whether `delete_after_import` is true or false.

### Sync execution classes

After config validation, each sync group is classified into one of three execution classes before any filesystem walking:

| Class | Applicable rules | delete_after_import | Behavior |
|-------|-----------------|---------------------|----------|
| `PassthroughNoDelete` | none | false | Direct unfiltered rsync, no walk, no metadata, no cleanup |
| `PassthroughDeleteAfterImport` | none | true | Direct unfiltered rsync plus durable cleanup capture for all entry kinds |
| `Purgatory` | non-empty | true | Walk, classify, upload manifest, server processing, status-based cleanup |

Passthrough groups do not participate in purgatory run config, server manifest, status, or processing. In mixed invocations, passthrough destinations are resolved separately from the purgatory run via `resolve-destinations`.

### No-rule groups (PassthroughNoDelete / PassthroughDeleteAfterImport)

If a sync group has no applicable postprocess rules:

- `delete_after_import = false` (PassthroughNoDelete): one direct unfiltered rsync to final storage. The source tree is not walked, entries are not classified, metadata is not read, and no bookkeeping is created.
- `delete_after_import = true` (PassthroughDeleteAfterImport): direct unfiltered rsync plus a durable cleanup ledger. Cleanup identity is captured per entry kind (size, mtime, and SHA-256 for regular files; link target for symlinks; subtree entries for directories) before rsync. Regular files without SHA identity are not deletion-authorizing. The cleanup state is written with `rsync_succeeded = false`. After rsync succeeds, `rsync_succeeded` is durably set to `true`. Only entries whose pre-rsync identity still matches are removed. New or modified entries created after cleanup capture are left untouched. No per-entry transfer filters are used.

### Match patterns and import modes

Each entry is classified as **passthrough** or **postprocessed** (purgatory) using only the rules applicable to its sync group.

- Passthrough entries are transferred directly to final server storage by a bulk rsync call. They have no server bookkeeping: no manifest entry, no receipt, no status entry.
- Postprocessed entries are transferred to the server's staging area, where subprocesses run before final commit. They are tracked in the server manifest and status.
- Covered entries are descendants of a postprocessed directory. They are not transferred independently. They appear in the server manifest and status as skipped.

The `match` value is an rsync include/exclude pattern. Rules are evaluated in order. **The first matching rule selects the entry for postprocessing with that rule's steps.** Later matching rules are ignored for that entry. If no rule matches, the entry is passthrough.

This first-match-wins selection applies consistently in:

- Client classification (manifest `postprocess_steps`)
- Server prepare-run validation
- Server postprocessing execution
- Planned output (path) validation
- Status `postprocess` step reporting

### Sync group classes

Every sync group is one of two classes:

- **Passthrough group**: no applicable postprocess rules. `delete_after_import` may be true or false. The group is handled entirely outside the purgatory run lifecycle.
- **Purgatory group**: one or more applicable postprocess rules. `delete_after_import` must be `true`. The group participates in the purgatory run (walk, manifest, upload, server processing).

Passthrough groups are not included in the uploaded `run.toml`, do not appear in the server manifest or status, and have no server-side bookkeeping of any kind.

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

Both calls call `rsync` with include/exclude filters.

The purgatory filter includes ancestor traversal directories needed to reach selected roots, then the roots themselves (exact or subtree), then excludes everything else.

### Identity-bearing bookkeeping

The client computes identity fields (size, mtime, SHA-256) only when they are needed for cleanup or server processing.

For ordinary passthrough entries in a sync group with `delete_after_import = false`:

- size is read from metadata for classification only (file type), not tracked for cleanup
- mtime_ns and SHA-256 are never computed
- no cleanup state is written
- the only operation is direct rsync

For ordinary passthrough regular files with `delete_after_import = true`:

- size, mtime_ns, and SHA-256 are computed for cleanup identity verification (SHA is required for delete-authorizing entries)
- durable cleanup state is written atomically to a stable state directory
- cleanup verifies local identity before deleting

### Cleanup authority

There are two distinct cleanup authorities:

1. **Transfer-confirmed cleanup** — applies to passthrough entries with `delete_after_import = true`. Local deletion is authorized by a durable cleanup state file atomically written after successful rsync. The client rechecks local identity (size, mtime, SHA-256 for regular files) before deleting. Missing required identity prevents deletion. This cleanup is not confirmed by server status.

2. **Server-confirmed cleanup** — applies to transformed/postprocessed entries. Local deletion is authorized by a valid server status file whose nickname and run ID match the original upload. The client verifies the status entry shows `imported` and the local identity still matches before deleting.

### Cleanup by entry kind

Cleanup identity is checked per entry kind:

- **Regular files**: size, mtime, and SHA-256 must match the captured identity. Missing SHA prevents deletion.
- **Symlinks**: the literal link target must match the captured identity. The symlink is unlinked without following the target. The target path is never modified.
- **Directories**: tracked descendants must still match their captured identities. Removal is bottom-up: child entries are removed first, then the directory itself. If new or changed entries appeared inside after identity capture, the directory is left in place.

### Import modes and cleanup

- **Passthrough entries with `delete_after_import = false`**: not cleaned locally. No identity bookkeeping.
- **Passthrough entries with `delete_after_import = true`**: transfer-confirmed cleanup. Cleaned locally after durable disk-backed state atomically records rsync success. Entry-kind identity checks apply.
- **Postprocessed entries**: server-confirmed cleanup. Removed after server status confirms the entry as imported. Entry-kind identity checks apply.

### Durable cleanup state

For `delete_after_import = true` passthrough, the client writes cleanup state to `state_dir`:

```text
{state_dir}/cleanup-{nickname}-{operation}.toml
```

The state file records:

- nickname and operation ID
- per-file entries with local path, size, mtime, SHA-256 (required for regular files), rsync success flag, and cleanup status

The state file is written atomically (temp file + rename). After each successful cleanup, the state is updated atomically. A crashed or interrupted cleanup resumes safely on the next `sync-and-cleanup` invocation: already-removed entries are idempotent, changed entries are skipped.

This is the only cleanup mechanism. It is used for both pure passthrough invocations and mixed invocations where some entries are postprocessed.

## Run config

The uploaded run configuration (`run.toml`) is a subset of the client config. It includes `nickname`, purgatory sync mappings (name + `to` path + `delete_after_import`), and postprocess rules. Only purgatory sync groups are included. Passthrough-only groups are resolved separately via `resolve-destinations`.

The postprocess rules in the purgatory run config are also filtered: only rules applicable to the included purgatory sync groups are present. Rules that only match passthrough-only sync groups are excluded.

```toml
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
```

The server validates that every sync in a purgatory run config has `delete_after_import = true`. If a purgatory run config includes a sync with `delete_after_import = false`, the run is rejected.

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
- **Symlink**: deletion is authorised when the literal link target matches. The symlink is unlinked without following the target. The target path is never modified.
- **Directory**: tracked descendants must still match their captured identities. If any regular-file descendant lacks SHA identity, the directory is not removed.

### SHA computation failure during classification

- Postprocess regular files: SHA failure is fatal during manifest building.
- Passthrough regular files in `delete_after_import = true` sync groups: SHA failure is fatal during manifest building when the entry is in a manifest that may authorise cleanup.
- Pure passthrough pre-rsync cleanup ledger capture: a regular file whose SHA cannot be computed is skipped (not added to the ledger) and a warning is logged.

## Trust boundary assumptions

Purgery's security model assumes:

1. **Client config** is trusted local admin configuration.
2. **Server config** is trusted server admin configuration.
3. **`server.command`** is a trusted configuration value (not user input).
4. **Postprocess programs and their argv** are trusted server-side configuration set by the server admin. Clients request steps by name but never upload arbitrary commands.
5. **Source filenames and paths** may contain arbitrary characters (spaces, special characters, leading `-`) and must not become shell syntax or local subprocess options. Purgery uses argv-style invocation and shell-escaping for remote commands to prevent injection.
6. **Local filesystem and server storage root** are non-hostile unless documented otherwise.

## Purgery-owned subprocess argv hardening

Purgery hardens its own `ssh` and `rsync` argv to prevent option injection:

| Subprocess | Pattern | Protection |
|------------|---------|------------|
| `ssh` | `ssh -- HOST COMMAND` | Prevents host value from being interpreted as an `ssh` option |
| `rsync` | `rsync [options...] -- SOURCE DEST` | Prevents source/destination paths from being interpreted as `rsync` options |

All `rsync` options, including filter merge files, appear before the `--` separator. Source and destination operands appear after it.

### Postprocess commands

Postprocess argv is trusted server-side configuration and is not rewritten, validated, or auto-fixed by Purgery. Purgery passes the configured `args` vector directly to the subprocess without modification.

Postprocess authors should use tool-appropriate `--` or option-value placement for path operands when their tools accept such syntax:

```toml
# Recommended: use -- before path operands
args = ["--input", "--", "{input}"]

# Alternative: prefix with ./ for relative-accepting tools
args = ["--input", "./{input}"]
```

## Client temp / filter directory layout

Client rsync filter files and temporary files live under `state_dir`:

```text
{state_dir}/tmp/{run_id}/filters/passthrough-{sync_name}
{state_dir}/tmp/{run_id}/filters/purgatory-{sync_name}
```

The directory is created before writing filters. Creation errors are reported, not silently ignored.

The `run_id` component ensures concurrent invocations with identical sync names do not collide. Leftover directories are harmless after a crash.

## Server work area layout

Server postprocess work areas live under `work_dir`:

```text
{work_dir}/{nickname}/processing/{run_id}/work/
```

This is inside the run's processing directory. On successful completion (`Done`), the work area is removed before the run moves to `done`. On failure (`Failed` or `Partial`), the work area stays with the run directory as it moves to `failed` or `done`. This ensures work areas are naturally cleaned or recovered with the processing run.
