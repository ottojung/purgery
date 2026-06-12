# Purgery Operations

## Setup and checks

```sh
purgery-server bootstrap --config /etc/purgery/server.toml
purgery-server check --config /etc/purgery/server.toml
purgery-client check --config ~/.config/purgery/client.toml
```

Bootstrap creates every configured server archive root and the server work directory. Checks validate configuration, executable resolution, and local prerequisites without importing data.

## Normal operation

```sh
purgery-server process-once --config /etc/purgery/server.toml
purgery-client sync-and-cleanup --config ~/.config/purgery/client.toml
purgery-server gc --config /etc/purgery/server.toml
```

A client invocation resolves each root-qualified `from` through client roots, selects entries using the optional `match`, and targets the root-qualified `to`. Sync groups are reported with generated IDs and their meaningful endpoints:

```text
sync-0001: videos/cats -> univ/videos/cats
sync-0002: configs -> sys/server-configs
sync-0003: videos -> univ/videos, match **/*dog*.png, postprocess compress-image
```

The nickname identifies operational run state only. Archive destinations and status `final_paths` never contain it.

## Direct passthrough and server runs

A sync without `postprocess` produces passthrough entries. The client can transfer passthrough-only groups directly to the server destination resolved from `to`. Cleanup-enabled passthrough transfers record identity and transfer completion durably before deleting originals.

A sync with `postprocess` stages selected entries in a server run. The server validates selected step names against `[postprocess.steps.*]`, verifies output safety, executes steps in order, commits outputs under the selected archive root, and reports root-qualified final paths. Postprocess syncs require cleanup.

Mixed invocations may combine direct passthrough groups and staged postprocess groups. Each group retains its generated sync ID and destination.

## Match-filtered operation

`match` is evaluated relative to the resolved client source. For:

```toml
[[sync]]
from = "videos"
to = "univ/videos"
match = "**/*dog*.png"
delete_after_import = true
```

`dogs/dog-beach.png` can be transferred and cleaned after confirmation. `dogs/readme.txt` and `trips/baku-arrival.mp4` remain untouched. Ancestor directories are transferred only as structural entries and are removed locally only if existing bottom-up safety rules permit it.

## Logging

```toml
[logging]
level = "info"       # error, warn, info, debug, trace
format = "pretty"    # pretty, compact, json
color = "auto"       # auto, always, never
```

CLI logging overrides take precedence over config values. User-facing sync messages should include the generated sync ID, `from`, and `to`.

## Heartbeats, leases, and garbage collection

Incoming runs carry a lease. Long-running client operations refresh the lease, and server processing writes progress heartbeats. Garbage collection may remove expired incoming runs but must not remove active leased work.

```toml
[gc]
incoming_lease_secs = 1800
heartbeat_interval_secs = 60
```

The heartbeat interval must leave sufficient margin before lease expiry. Recovery resumes processing runs from durable phase state. Completed and tombstoned runs remain authoritative for cleanup decisions.

## Trust and executable resolution

`server.command` is an administrator-controlled SSH command prefix. Purgery does not treat client-provided postprocess strings as commands: executable definitions exist only in server configuration. Subprocesses are invoked with explicit argv, not through a shell.

## Final-storage overlay

Imports overlay selected entries into existing archive trees. Unrelated archive entries are preserved. Final paths are formed as:

```text
<server root path>/<to path under root>/<entry path relative to resolved source>
```

For example, `videos/cats` to `univ/videos/cats` maps `kitten-01.mp4` to `/universe/synced/videos/cats/kitten-01.mp4`.
