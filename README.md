# Purgery

Purgery imports generated filesystem entries from devices into a central archive, optionally transforms them, and only removes local originals when doing so is explicitly configured and safe.

You have photos, videos, recordings, or other generated files on a laptop, camera SD card, or similar device. They fill up local storage. You want to move them into a central archive — and maybe compress or convert them on the way — without risking data loss.

Purgery is the import pipeline for that.

## Non-goals

Purgery is not bidirectional sync, not a Dropbox/Syncthing replacement, not a network daemon, not a multi-user authorization system, not a remote shell execution framework, and not an automatic conflict-resolution system. It is intentionally a one-way import pipeline.

## Quick start

### Server

```sh
# Create archive root directories
purgery-server bootstrap --config server.toml

# Verify configuration and dependencies
purgery-server check --config server.toml

# Run one batch of imports
purgery-server process-once --config server.toml
```

### Client (source device)

```sh
# Verify local executables and configuration (no SSH)
purgery-client check --config client.toml

# Run a full import cycle: transfer, transform, clean up confirmed originals
purgery-client sync-and-cleanup --config client.toml
```

## How it works

### Terminology

| Term | Meaning |
|------|---------|
| **Source tree** | A local directory whose contents you want to import (e.g., `/home/user/Videos`) |
| **Archive** | The central storage location where imported files accumulate (a path on a server) |
| **Import** | The act of copying or transforming an entry from a source tree into the archive |
| **Transform** | An optional server-side postprocessing step (e.g., compress, convert, rename) applied during import |
| **Passthrough import** | Copying a file directly into the archive without transformation |
| **Transformed import** | Copying a file into the archive through a server-side transformation step |
| **Cleanup** | Removing a confirmed local original file after import is complete and verified |

---

1. You configure one or more **source trees** on a device and point each to a destination inside the archive.
2. The client walks each source tree (never following symlinks) and classifies every entry as either **passthrough** (direct copy to archive) or **transformed** (server-side processing required).
3. If any source tree has transformed entries, the client creates a server run: it uploads a manifest of only the entries needing transformation, validates the plan on the server, and transfers them to a staging area.
4. The server processes transformed entries (prepares work areas, runs subprocesses, commits outputs) and writes a status record.
5. For source trees that are pure passthrough (no transformation), the client skips server bookkeeping entirely and copies files directly to the archive.
6. Local cleanup of originals depends on how cleanup was configured for each source tree. Passthrough files use locally recorded transfer state as authority. Transformed files are cleaned only after server status confirms the import.

## Configuration

Minimal server config (`server.toml`):

```toml
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
```

Minimal client config (`client.toml`):

```toml
nickname = "laptop"

[server]
host = "example.com"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
```

Full config reference: [docs/config.md](docs/config.md)

## Transforms (postprocessing)

Transformations are defined on the server. Clients request named steps by rule; they do not upload arbitrary commands.

```toml
# server.toml
[postprocess.steps.compress-video]
kind = "subprocess"
program = "/usr/local/bin/compress"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.compressed.webm"]
keep_original = true
```

```toml
# client.toml
[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
```

## Transform and cleanup coupling

Because transformed outputs are not the original source files, Purgery cannot use the final archive alone to know that an unchanged local original has already been processed in a previous run. The server does not retain indefinite source-file metadata or an ever-growing receipt ledger.

For this reason, a source tree with transforms **must** also enable cleanup (`delete_after_import = true`). The transformed import is an import-and-retire operation:

1. the source entry is uploaded into a server run;
2. the server transforms and commits outputs;
3. the server writes a bounded run status;
4. the client removes the unchanged local original after server-confirmed import.

This prevents repeated reprocessing of the same original on subsequent runs.

A source tree without transforms may still use `delete_after_import = false` — passthrough imports preserve the original content in the archive, so repeated runs converge naturally.

## Safety model

Purgery targets Unix/POSIX filesystem semantics and is conservative about data loss:

- For **passthrough imports**, cleanup is opt-in per source tree (`delete_after_import = true`). A passthrough source tree with `delete_after_import = false` does not clean up local originals.
- For **transformed imports**, cleanup is required by the conformance model. Because the server does not retain indefinite source-file metadata and transformed outputs are not the original files, the source original must be retired locally after successful import (import-and-retire). See [Transform and cleanup coupling](#transform-and-cleanup-coupling).
- **Transformed imports**: cleanup is server-confirmed. The client removes local originals only after the server confirms the import in a valid status record whose nickname and run ID match the original upload.
- **Passthrough imports with delete-after-import**: cleanup is transfer-confirmed. A durable local state file is atomically recorded after successful transfer to the archive. The client verifies the local entry still matches its uploaded identity before removal.
- **Passthrough imports without delete-after-import**: no cleanup occurs. The local entry remains after transfer.
- Before any removal, the client verifies the local entry still matches its uploaded identity (size, mtime, optional SHA-256 for regular files; link target for symlinks; subtree identity for directories).
- The server performs a recursive merge into the archive: directories merge, regular files replace existing ones, symlinks remain symlinks, and absent source entries never delete archive entries.
- Symlink targets are literal data. The server never follows staged or archive symlinks as directories.
- Tree imports provide replayable convergence through crash-safe per-entry commits, not an all-or-nothing transaction.
- Transforms apply to directories, regular files, and symlinks. Client cleanup remains conservative and removes only confirmed unchanged local originals, respecting entry-kind identity checks (symlinks are unlinked without following the target; directories are removed bottom-up only when safe).
- Overlapping source trees that would produce the same archive path are rejected rather than resolved by ordering.

## More documentation

- [Config reference](docs/config.md) — archive, client, transform, and run configuration
- [Protocol](docs/protocol.md) — lifecycle, subcommands, run states, status format
- [Operations](docs/operations.md) — bootstrap, check, GC, heartbeat, leases
- [Import semantics](docs/design/import-semantics.md) — tree-overlay model, work areas, and per-entry safety rules
- [Rsync overlay oracle](docs/design/rsync-overlay-oracle.md) — characterized conflict cases and intentional Purgery differences
- [Crash safety and idempotence](docs/design/crash-safety-and-idempotence.md) — durable phases, replay recovery, atomic replacement, and deletion authority
