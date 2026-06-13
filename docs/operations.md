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

Server checks: parse config, verify `work_dir` exists (but does not create it), resolve every postprocess `program`, validate step invariants.

If server directories do not exist, `check` reports an error.

## Normal operation

```sh
purgery-server process-once --config server.toml

purgery-client sync -- ~/video.mp4 user@server:/archive

purgery-client sync \
  --postprocess compress-video \
  --delete-after-import \
  -- ~/Videos/trip user@server:/archive

purgery-client sync \
  --postprocess compress-video \
  --delete-after-import \
  --split "**/*.mp4" \
  -- ~/Videos user@server:/archive

purgery-server gc --config server.toml
```

`process-once` runs side-effect-free server validation first, then GC opportunistically, then recovers processing runs and processes ready runs.

## Split

The `--split <PATTERN>` flag selects source entries to process individually. The pattern uses rsync-style single-pattern matching as a positive selector. It supports the documented rsync include/exclude pattern forms without implementing a full ordered rule list.

### Pattern syntax

- Patterns with `/` match against relative paths.
- Patterns without `/` may match any path component.
- `*` matches within a single directory level (does not cross `/`).
- `**` matches across directory boundaries.
- Trailing `/` restricts to directories only.
- Leading `/` anchors to the transfer root (`<SOURCE>`).
- `?` matches any single character except `/`.

### Pattern matching behavior

- `--split` is a positive selector, not an ordered include/exclude filter list.
- The source entry itself is candidate `.`.
- Every descendant is a candidate, represented by its normalized relative path from `<SOURCE>`.
- Candidates are tested against the single pattern. If a matched entry has a matched ancestor, only the ancestor is kept (ancestor pruning).
- The result is a deterministic non-overlapping set of roots in normalized path order.

### No-match behavior

If `--split <PATTERN>` matches nothing, an info-level message is logged, no transfer is performed, no server run is created, and the client exits successfully with status code 0.

### Target suffix

Each matched root gets a target suffix that preserves its relative layout under `<TARGET>`:

| Matched entry | Suffix | Example target |
|---------------|--------|-------|
| `<SOURCE>` exactly | empty | `user@host:/archive` |
| Top-level child of `<SOURCE>` | `/` | `user@host:/archive/` |
| Nested child | `/parent` | `user@host:/archive/parent` |

### Pure passthrough optimization

Without `--delete-after-import` or `--postprocess`, the split is implemented as a single rsync transfer that copies only the selected roots and their payloads. Nested entries preserve their relative parent paths under `<TARGET>`. No server run or client state is created. No destination collision preflight is added.

### Serialized split for cleanup and postprocess

With `--delete-after-import` or `--postprocess`, each matched entry is processed as a serialized non-split sync operation. The next operation starts only after the previous one is completely done: transfer finished, server run reached terminal (if postprocess), status read, cleanup confirmed, local deletion completed.

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
```

## Server-side GC

```sh
purgery-server gc --config server.toml
```

GC scans incoming directories for expired runs. A run is expired if:

1. Its `lease.toml` exists and `expires_at_unix_secs` is in the past.
2. No lease exists and the directory mtime is more than `2 × incoming_lease_secs` old.

Collection process:

1. Rename `incoming/<run_id>` → `failed/<run_id>` (atomic claim).
2. Write `status.toml` with `state = "failed"` and appropriate error message.
3. Remove `files/` to reclaim disk.
4. Keep metadata: `lease.toml`, `run.toml`, `manifest.toml`, `status.toml`.

If `failed/<run_id>` already exists, the abandoned run is moved to a GC quarantine path instead of merging directories. The same status and file cleanup is applied to quarantined runs.

GC is run opportunistically at the start of `process-once` and `begin-run`. It is never run from `check`. Expose separately for cron/systemd timers.

## `--server-command` trust model

The client's `--server-command` value is a trusted command name executed on the remote host via SSH. Purgery appends shell-escaped arguments. This is not intended to accept untrusted input.

## Executable resolution

Executable resolution follows these rules:

- **Absolute path**: follow symlinks, require target exists and is a regular file, require executable bit set on Unix.
- **Relative name**: searched in `PATH`; follow symlinks, require target is regular file, require executable bit set.
- **Directories** are rejected. **Broken symlinks** are rejected.

This is used for client `ssh`, `rsync`, and server postprocess `program` values.

## Restart recovery

`process-once` recovers runs already in `processing/` before claiming runs from `ready/`. Operators do not need to move phase directories manually after a crash. Recovery uses staged files and filesystem status only; see [Crash Safety and Idempotent Imports](design/crash-safety-and-idempotence.md).

## Final-storage overlay

Each run overlays its uploaded source onto the destination with recursive archive-mode rsync semantics and no delete option. The source entry name is appended to the destination to form the final path. Existing directories are merged, regular files and symlinks replace compatible destination entries.

Symlink targets are stored and recreated literally. Neither staged symlinks nor destination symlinks are traversed as directories. Postprocessing applies to the source entry, regardless of kind.

A crash can expose a prefix of the entry overlay. This is expected: the run remains in `processing/` without a terminal status and `process-once` replays it until the final tree converges. The operation is not an all-or-nothing filesystem transaction.
