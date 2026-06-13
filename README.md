# Purgery

Purgery imports filesystem entries from devices into a destination, optionally transforms them on the server, and only removes local originals when doing so is explicitly configured and safe.

## Quick start

### Server

```sh
purgery-server check --config /etc/purgery/server.toml
purgery-server process-once --config /etc/purgery/server.toml
```
If `--config` is omitted, the server looks for config at `$PURGERY_SERVER_CONFIG_PATH`, `$XDG_CONFIG_HOME/purgery/server.toml`, `~/.config/purgery/server.toml`, or `/etc/purgery/server.toml`.

### Client (source device)

```sh
purgery-client sync -- ~/Videos user@server:/universe/synced/videos

purgery-client sync \
  --postprocess compress-video \
  --delete-after-import \
  -- ~/Videos user@server:/universe/synced/videos
```

SOURCE must be a local directory. The destination is rsync-style: `USER@HOST:/absolute/path` or `USER@HOST:relative/path`. Both absolute and relative destinations are accepted. For postprocess runs, a relative destination is resolved against the server's working directory during `prepare-run` and the resolved absolute path persists in the run.

## How it works

1. You invoke `purgery-client sync` with a local source tree and a remote destination.
2. Without `--postprocess`, the client transfers directly to the destination with rsync; no server run or manifest exists.
3. With passthrough cleanup, the client records local identity before rsync and removes only unchanged originals after rsync succeeds.
4. With `--postprocess`, the client validates a server run plan before staging files, waits for processing, and retires only unchanged originals that server status marks imported.

## Configuration

Minimal server config (`server.toml`):

```toml
work_dir = "/var/lib/purgery/work"
```

Server config contains only server-owned concerns: work directory, postprocess step definitions, GC settings, and logging.

Full config reference: [docs/config.md](docs/config.md)

## Transforms (postprocessing)

Transformations are defined on the server. Clients request named steps via the `--postprocess` flag. Postprocessing applies only to regular files; directories and symlinks are treated as passthrough regardless of `--postprocess`.

```toml
# server.toml
[postprocess.steps.compress-video]
kind = "subprocess"
program = "/usr/local/bin/compress"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.compressed.webm"]
keep_original = true
```

## Transform and cleanup coupling

Because transformed outputs are not the original source files, Purgery cannot use the final archive alone to know that an unchanged local original has already been processed in a previous run.

For this reason, `--postprocess` requires `--delete-after-import`. The transformed import is an import-and-retire operation:

1. the source entry is uploaded into a server run;
2. the server transforms and commits outputs;
3. the server writes a bounded run status;
4. the client removes the unchanged local original after server-confirmed import.

This prevents repeated reprocessing of the same original on subsequent runs.

A passthrough import may still use `--delete-after-import` — passthrough imports preserve the original content, so cleanup is optional.

## Safety model

Purgery targets Unix/POSIX filesystem semantics and is conservative about data loss:

- **Passthrough imports**: cleanup is opt-in. With `--delete-after-import`, a durable local state file records the transfer; the client verifies the local entry still matches its uploaded identity before removal.
- **Transformed imports**: cleanup is required (`--delete-after-import`). The client removes local originals only after the server confirms the import in a valid status record.
- Before any removal, the client verifies the local entry still matches its uploaded identity (size, mtime, optional SHA-256 for regular files; link target for symlinks; subtree identity for directories).
- The server performs a recursive merge into the destination: directories merge, regular files replace existing ones, symlinks remain symlinks, and absent source entries never delete destination entries.
- Symlink targets are literal data. The server never follows staged or destination symlinks as directories.
- Tree imports provide replayable convergence through crash-safe per-entry commits, not an all-or-nothing transaction.

## More documentation

- [Config reference](docs/config.md) — server config, transform definitions, run configuration
- [Protocol](docs/protocol.md) — lifecycle, subcommands, run states, status format
- [Operations](docs/operations.md) — check, GC, heartbeat, leases
- [Import semantics](docs/design/import-semantics.md) — tree-overlay model, work areas, and per-entry safety rules
- [Crash safety and idempotence](docs/design/crash-safety-and-idempotence.md) — durable phases, replay recovery, atomic replacement, and deletion authority
