# Purgery Operations

## Logging

Purgery uses the `tracing` framework for structured logging. All log output goes to **stderr**; stdout is reserved for machine-readable protocol output (e.g., `begin-run` and `status` print TOML to stdout).

### Configuration

Logging can be configured in the TOML config file:

```toml
[logging]
level = "info"       # error | warn | info | debug | trace
format = "pretty"    # pretty | compact | json
color = "auto"       # auto | always | never
```

### CLI overrides

Both binaries support global flags that override config file and environment:

| Flag | Effect |
|------|--------|
| `--log-level <level>` | Override log level |
| `--log-format <format>` | Override log format |
| `--color <mode>` | Override color mode |
| `--quiet` | Set level to `error` (conflicts with `--verbose` and `--log-level`) |
| `--verbose` | Set level to `debug` (conflicts with `--quiet` and `--log-level`) |

Precedence: CLI flags > config file > default. The `RUST_LOG` environment variable is not consulted; logging is controlled entirely through the config file and CLI flags.

## Server setup

```sh
purgery-server check --config server.toml
```

Server checks: parse config, verify `work_dir` exists (but does not create it), resolve every transform `program`, validate transform invariants.

If server directories do not exist, `check` reports an error.

## Normal operation

```sh
purgery-client sync -- ~/video.mp4 user@server:/archive

purgery-client sync \
  --transform compress-video \
  --delete-after-import \
  -- ~/Videos/trip user@server:/archive

purgery-client sync \
  --transform compress-video \
  --delete-after-import \
  --split "**/*.mp4" \
  -- ~/Videos user@server:/archive
```

When `purgery-client sync` is invoked with `--transform`, the client uploads the run and finishes it, then spawns a foreground remote `purgery-server process-run --nickname <nickname> --run-id <run-id>` in a local supervised handle while concurrently polling `run-state`. The client restarts `process-run` on SSH transport failure when needed, and reads status after terminal `run-state`. Transform sync is synchronous and may wait indefinitely until the server run reaches a terminal state. A separate daemon, cron job, or timer is not required for the basic synchronous transform path.

Before the server run begins, the client initiates `purgery-server gc` as a best-effort foreground command. GC failure is logged and does not fail the transform sync. This is the only client-initiated GC for the invocation.

`process-run` does not run GC.

The target run is driven independently of unrelated ready/processing runs. `process-run` may claim the target from `ready`, recover an abandoned target in `processing`, observe that another processor is actively handling it, or observe that it is already terminal. It does not process unrelated ready/processing runs.

This makes a single client-triggered transform sync self-contained: the server processing happens during the client invocation. The client does not require a separately running daemon, cron job, or timer for the normal transform path.

Operators who want a daemon or timer to process all queued runs, recover abandoned work, or perform batch maintenance may run:

```sh
purgery-server process-once --config server.toml
```

This runs GC, recovers unlocked processing runs (respecting active processor locks), and processes all ready runs. It is independent of client-triggered `process-run` and is no longer required for a normal synchronous transform client.

Each processing run has a `processor.lock` file (using `flock`) that prevents concurrent mutation. A run may be mutated only by a process holding its lock. If the lock is held by another process, the run is considered actively owned and will not be recovered or replayed. A busy lock is normal concurrency, not an error. Terminal directories do not retain `processor.lock`.

Passthrough syncs (without `--transform`) use direct rsync and do not call any server processing command.

### Source entry model

The `SOURCE` operand may be a regular file, directory, or symlink. The target is a destination parent. The source entry is imported under the target using the source entry name.

- Trailing slashes on source operands do not change source-entry semantics. `~/Videos` and `~/Videos/` both import the directory named `Videos`.
- `.` imports the current directory as a source entry named after that directory. `.` is resolved to a concrete directory path before rsync.
- `..` imports the parent directory as a source entry named after that directory. `..` is resolved to a concrete directory path before rsync.
- `/` is invalid in every mode — it has no source entry name.
- Symlink sources remain symlink sources. Source paths are not canonicalized through symlinks.

The source entry name is used consistently for the manifest `relative_path`, staged path, cleanup root, server expected staged path, and server base final path (`<destination>/<source_entry_name>`).

## Split

The `--split <PATTERN>` flag selects source entries to process individually. The pattern is an rsync-style single positive selector, not an ordered include/exclude rule list.

Split operates in one of two modes depending on the presence of `--delete-after-import` or `--transform`.

### Pattern syntax

- Patterns with `/` match against relative paths.
- Patterns without `/` may match any path component.
- `*` matches within a single directory level (does not cross `/`).
- `**` matches across directory boundaries.
- Trailing `/` restricts to directories only.
- Leading `/` anchors to the transfer root (`<SOURCE>`).
- `?` matches any single character except `/`.

### Pure passthrough split

Without `--delete-after-import` or `--transform`, the client performs one rsync filter transfer with constant include/exclude rules derived from the pattern. No Purgery-side candidate discovery, ancestor pruning, or root ordering is performed. The contract is final destination effect under the generated rsync filter rules.

Rsync filter rules (actual argv, not shell-quoted):

```
--include=*/
--include=<P-as-directory-payload>
--include=<P-as-nested-directory-payload>   (patterns without "/" only)
--include=<P-as-entry>
--exclude=*
```

When shown as shell examples, filter arguments are quoted so the shell does not expand wildcards. The actual process argv values must not contain those quote characters.

```
rsync -a -m \
  --include='*/' \
  --include='<P-as-directory-payload>' \
  --include='<P-as-nested-directory-payload>' \
  --include='<P-as-entry>' \
  --exclude='*' \
  -- <SOURCE>/ <HOST>:<TARGET>/
```

`<P-as-entry>` is the pattern verbatim. `<P-as-directory-payload>` appends `/***` to the pattern (stripping a trailing `/` if present), ensuring that top-level matched directories transfer their full payload. `<P-as-nested-directory-payload>` is the same but prefixed with `**/`, ensuring that directories whose names match a component-only pattern at any nesting depth transfer their full payload. This rule is only emitted for patterns without `/`.

The source operand in filter mode intentionally has a trailing slash (`<SOURCE>/`) so selected entries land directly under `<TARGET>` rather than under `<TARGET>/<SOURCE-NAME>`. Nested entries preserve their relative parent paths under `<TARGET>`.

`--split "."` is special: it uses ordinary source-entry rsync (no trailing slash on source, no filter rules) and imports `<SOURCE>` as `<TARGET>/<SOURCE-NAME>`.

Pure passthrough split uses `--prune-empty-dirs` to remove traversal-only directory scaffolding created by the `*/` rule. Empty directories selected only by the filter may not be created at the destination. Cleanup and transform split do not use this optimization.

No server run, manifest, or client state is created. No destination collision preflight is performed.

For directory sources, rsync always runs for non-dot patterns; when nothing matches the filter, rsync transfers nothing. For non-directory sources (regular files, symlinks), only `--split "."` can match; other patterns are no-op and exit successfully without invoking rsync.

### Serialized split for cleanup and transform

With `--delete-after-import` or `--transform`, the client discovers matching entries using Purgery's own pattern matcher. Match determination uses these rules:

- `<SOURCE>` itself is candidate `.` (matched by its relative sentinel, not by basename).
- Every descendant is a candidate, represented by its normalized relative path from `<SOURCE>`.
- Candidates are tested against the single pattern. If a matched entry has a matched ancestor, only the ancestor is kept (ancestor pruning).
- The result is a deterministic non-overlapping set of roots in normalized path order.

Each matched root gets a target suffix that preserves its relative layout under `<TARGET>`:

| Matched entry | Suffix | Example target |
|---------------|--------|-------|
| `<SOURCE>` exactly | empty | `user@host:/archive` |
| Top-level child of `<SOURCE>` | `/` | `user@host:/archive/` |
| Nested child | `/parent` | `user@host:/archive/parent` |

Each root is processed as a serialized non-split sync operation. The next operation starts only after the previous one is completely done: transfer finished, server run reached terminal (if transform), status read, cleanup confirmed, local deletion completed.

If the pattern matches nothing, an info-level message is logged, no transfer is performed, no server run is created, and the client exits successfully with status code 0.

## Heartbeat and leases

When `begin-run` creates an incoming directory, it writes a `lease.toml` file:

```toml
protocol_version = 1
nickname = "laptop"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
created_at_unix_secs = 1234567890
last_heartbeat_unix_secs = 1234567890
expires_at_unix_secs = 1234569690
```

The client keeps the incoming lease fresh by calling `heartbeat-run` at the configured interval from `begin-run` through `finish-run`. If the lease expires before the server accepts the run (`finish-run` completes), the server may GC the incoming run. If a heartbeat call fails and `finish-run` has not yet succeeded, the client aborts the operation. Once `finish-run` succeeds the run has left `incoming` and heartbeat failure is no longer fatal.

The heartbeat updates `last_heartbeat_unix_secs` and extends `expires_at_unix_secs` by `incoming_lease_secs`.

### GC config

```toml
[gc]
incoming_lease_secs = 1800
heartbeat_interval_secs = 60
ready_retention_secs = 3600
processing_retention_secs = 3600
done_retention_secs = 3600
failed_retention_secs = 86400
orphan_retention_secs = 86400
```

## Server-side GC

```sh
purgery-server gc --config server.toml --log-level debug
```

GC collects expired server work state across `incoming`, `ready`, `processing`, `done`, and `failed`. `work_dir` is temporary operational state: it is not final storage, an archive, or a permanent history database.

Terminal retention (`done`, `failed`) is measured from `status.toml` mtime (the last modification time of the published status file). This provides a stable clock that is not extended by GC's own pruning of payload or control material. Without a valid `status.toml`, terminal directories fall under orphan retention.

`incoming` uploads expire by lease. Lease expiry is trusted only after validating that the lease has compatible `purgery_version`, matching `protocol_version`, and matching nickname/run id. Incompatible, protocol-mismatched, or malformed leases are governed by orphan retention instead — their `expires_at_unix_secs` is never trusted.

`ready` requests are queued work and expire after `ready_retention_secs` measured from directory mtime. `processing` requests are recoverable while unlocked and inside `processing_retention_secs`; locked processing is never deleted underneath an active holder, but an expired locked request produces a warning and the next GC after the lock disappears can remove it. GC may delete expired unlocked processing state before recovery is attempted. Operators who want a longer crash-recovery window for abandoned processing runs should increase `processing_retention_secs`.

Successful `done` requests keep only bounded terminal metadata (`status.toml`, `run.toml`, and `manifest.toml`) for `done_retention_secs`, measured from `status.toml` mtime. Successful terminal state does not retain `files/`, `work/`, leases, progress, processor locks, or temporary `*.tmp` files. Pruning within a `done` run does not refresh its retention clock. Expired terminal directories are deleted entirely. Failed requests may retain more material for `failed_retention_secs` (from `status.toml` mtime) so operators can inspect failures, then the entire failed directory is deleted. Malformed, incompatible, or unknown request directories are treated as orphans: fresh state is retained for `orphan_retention_secs`, logged with a warning, and old orphan state is removed. Invalid top-level nickname directories under `work_dir` are also governed by `orphan_retention_secs` and are never silently skipped.

At `debug` or `trace`, GC logs meaningful decisions with client nickname, run id, phase, whether the request is retained or expired, seconds until deletion or past expiry, the action taken, and skip reasons such as an active processing lock. GC is run by `process-once` before batch recovery/processing. Transform sync clients also initiate GC before `begin-run` as a best-effort foreground command.

## `--server-command` trust model

The client's `--server-command` value is a trusted command name executed on the remote host via SSH. Purgery appends shell-escaped arguments. This is not intended to accept untrusted input.

## Executable resolution

Executable resolution follows these rules:

- **Absolute path**: follow symlinks, require target exists and is a regular file, require executable bit set on GNU.
- **Relative name**: searched in `PATH`; follow symlinks, require target is regular file, require executable bit set.
- **Directories** are rejected. **Broken symlinks** are rejected.

This is used for client `ssh`, `rsync`, and server transform `program` values.

## Restart recovery

`process-once` recovers runs already in `processing/` before claiming runs from `ready/`. Operators do not need to move phase directories manually after a crash. Recovery uses staged files and filesystem status only; see [Crash Safety and Idempotent Imports](design/crash-safety-and-idempotence.md).

## Publication semantics

Purgery has different publication contracts in different modes.

### Direct passthrough

Direct passthrough is rsync-style publication. The client invokes rsync directly against the destination. No server run is created, no manifest is uploaded, and no terminal server status exists.

This is not atomic publication. Interrupted transfers can leave partial contents at the exact destination path according to rsync behavior and the flags Purgery uses. In direct passthrough with `--delete-after-import`, successful rsync exit is the import confirmation used to authorize local cleanup.

### Transform runs

Transform runs are server-run/status-tracked operations. The server records terminal status after processing.

For transform outputs, Purgery still does not perform atomic final publication. The transform program writes final outputs itself. Purgery checks declared `expected_outputs` after successful subprocess exit and records those paths in status. If atomic output matters, the transform program must implement it.

## Destination effects

Publication to the destination differs by run mode:

- **Direct passthrough (no transform):** rsync writes the source entry directly to `<destination>/<source_entry_name>` with archive-mode semantics. Existing directories merge, regular files and symlinks replace destination entries. Non-atomic: interrupted rsync can leave partial contents at the destination path.

- **Transform run:** The transform program writes outputs directly to the resolved expected output paths. Purgery checks that each declared `expected_outputs` exists after successful subprocess exit and records those paths in status. The source entry is consumed by the transform flow. If `expected_outputs = []`, successful subprocess exit alone is sufficient; no output-existence check is performed.

In direct passthrough, symlink sources are transferred as symlinks with literal targets. In transform runs, any symlink outputs are created by the transform program; Purgery only checks that declared symlink outputs exist. Neither staged symlinks nor destination symlinks are traversed as directories.

For transform runs, a crash can expose partial output at the destination. The run remains in `processing/` without a terminal status and `process-once` replays it until the transform completes successfully. The operation is not an all-or-nothing filesystem transaction.
