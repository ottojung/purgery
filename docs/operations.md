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

## Setup

```sh
# Create root and purgery_root directories
purgery-server bootstrap --config server.toml

# Verify configuration and dependencies
purgery-server check --config server.toml
```

`bootstrap` creates `root` and `purgery_root` if missing. It does not process runs or run GC.

## Boot-time checks

Both binaries support a `check` subcommand that is local and side-effect-free — no SSH, no directory creation, no mutations.

```sh
purgery-client check --config client.toml
purgery-server check --config server.toml
```

Client checks: parse config, resolve `ssh` and `rsync` executables, validate config fields.

Server checks: parse config, verify `root` and `purgery_root` exist (but do not create them), resolve every postprocess `program`, validate step invariants.

If server directories do not exist, `check` reports an error and suggests running `bootstrap` first.

## Normal operation

```sh
# Server: recover processing runs and process ready runs
purgery-server process-once --config server.toml

# Client: sync files and clean up confirmed imports
purgery-client sync-and-cleanup --config client.toml

# Server: run garbage collection manually
purgery-server gc --config server.toml
```

`process-once` runs side-effect-free server validation first, then GC opportunistically, then recovers processing runs and processes ready runs.

`sync-and-cleanup` runs local checks first, then uploads, waits for processing, and cleans up confirmed local entries.

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

During `sync-and-cleanup`, the client spawns a background thread that calls `heartbeat-run` at the configured interval, covering the entire upload phase including long single rsync transfers. If the heartbeat thread fails, the client aborts before `finish-run`.

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

## `server.command` trust model

The client's `[server].command` value is a trusted shell command prefix executed on the remote host via SSH. Purgery appends shell-escaped arguments. This is not intended to accept untrusted input.

## Executable resolution

Executable resolution follows these rules:

- **Absolute path**: follow symlinks, require target exists and is a regular file, require executable bit set on Unix.
- **Relative name**: searched in `PATH`; follow symlinks, require target is regular file, require executable bit set.
- **Directories** are rejected. **Broken symlinks** are rejected.

This is used for client `ssh`, `rsync`, and server postprocess `program` values.

## Restart recovery

`process-once` recovers runs already in `processing/` before claiming runs from `ready/`. Operators do not need to move phase directories manually after a crash. Recovery uses staged files and filesystem status only; see [Crash Safety and Idempotent Imports](design/crash-safety-and-idempotence.md).

## Final-storage overlay

Each run overlays its uploaded tree onto final storage with recursive archive-mode rsync semantics and no delete option. Existing directories are merged, regular files and symlinks replace compatible destination entries, and extra final descendants remain. A regular file or symlink does not replace a non-empty destination directory. Source directory entries can replace conflicting file or symlink parents before descendants are imported.

Symlink targets are stored and recreated literally. Neither staged symlinks nor final-storage symlinks are traversed as directories. Postprocessing applies to regular files, directories, and symlinks. Client cleanup removes only confirmed unchanged local originals, respecting entry-kind identity checks.

A crash can expose a prefix of the entry overlay. This is expected: the run remains in `processing/` without a terminal status and `process-once` replays it until the final tree converges. The operation is not an all-or-nothing filesystem transaction.
