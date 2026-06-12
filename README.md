# Purgery

Purgery is a one-way client/server import tool. A client selects entries from named local roots, uploads them with rsync, and optionally asks the server to run server-owned postprocessing steps. Local originals are removed only when cleanup is enabled and the server or durable direct-transfer state confirms the selected entry was imported.

Purgery is not bidirectional synchronization, a network daemon, or an automatic conflict resolver.

## Configuration model

Both sides use named roots:

```text
from = "<client-root-name>/<path-under-client-root>"
to   = "<server-root-name>/<path-under-server-root>"
```

Client roots identify local source trees. Server roots identify archive destinations. A sync group connects one root-qualified source to one root-qualified destination.

### Server

```toml
work_dir = "/var/lib/purgery/work"

[[root]]
name = "univ"
path = "/universe/synced"

[[root]]
name = "sys"
path = "/etc/system"

[postprocess]

[postprocess.steps.compress-image]
kind = "subprocess"
program = "my-compress-image"
args = ["--input", "{input}"]
expected_outputs = ["{file_stem}.jpeg"]
keep_original = false
```

The server owns archive roots, executable programs, argument templates, expected output names, final-path resolution, and path-within-root validation.

### Client

```toml
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"

[[root]]
name = "videos"
path = "/home/user/Videos"

[[root]]
name = "configs"
path = "/home/user/my/server-configs"

[[sync]]
from = "videos/cats"
to = "univ/videos/cats"

[[sync]]
from = "configs"
to = "sys/server-configs"
delete_after_import = true

[[sync]]
from = "videos"
to = "univ/videos"
match = "**/*dog*.png"
postprocess = ["compress-image"]
delete_after_import = true
```

`from = "videos/cats"` resolves to `/home/user/Videos/cats`. `to = "univ/videos/cats"` resolves on the server to `/universe/synced/videos/cats`. A source entry `kitten-01.mp4` therefore lands at `/universe/synced/videos/cats/kitten-01.mp4`.

The client nickname is operational metadata for run directories and diagnostics. It never appears in direct rsync destinations, status `final_paths`, or archive paths.

Sync groups receive deterministic internal IDs in config order: `sync-0001`, `sync-0002`, and so on. Configuration authors identify a sync by its `from` and `to` values rather than by a separate name.

## Selection and postprocessing

When `match` is omitted, all entries under the resolved source are selected. When present, it uses Purgery's rsync-style matcher against normalized paths relative to that source. Matching files and symlinks are selected; ancestor directories needed to materialize them are structural entries. Unmatched entries are neither uploaded nor eligible for cleanup.

`postprocess` is an ordered, non-empty list of server-defined step names. It is written directly on a sync group. A sync with postprocessing must set `delete_after_import = true`. The client does not define commands, arguments, expected outputs, or `keep_original` behavior.

Status paths remain root-qualified and relative:

```text
univ/videos/cats/kitten-01.mp4
sys/server-configs/nginx/site.conf
univ/videos/dogs/dog-beach.jpeg
```

## Commands

```sh
cargo build --workspace

purgery-server bootstrap --config /etc/purgery/server.toml
purgery-server check --config /etc/purgery/server.toml
purgery-server process-once --config /etc/purgery/server.toml

purgery-client check --config ~/.config/purgery/client.toml
purgery-client sync-and-cleanup --config ~/.config/purgery/client.toml
```

See [configuration](docs/config.md), [operations](docs/operations.md), [protocol](docs/protocol.md), and [import semantics](docs/design/import-semantics.md).
