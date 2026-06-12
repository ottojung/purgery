# Purgery Configuration

Purgery parses configuration strictly: unknown fields and invalid boundary values are errors.

## Config discovery

Server lookup order:

1. `--config PATH`
2. `$PURGERY_SERVER_CONFIG_PATH`
3. `$XDG_CONFIG_HOME/purgery/server.toml`
4. `$HOME/.config/purgery/server.toml`
5. `/etc/purgery/server.toml`

Client lookup order:

1. `--config PATH`
2. `$PURGERY_CLIENT_CONFIG_PATH`
3. `$XDG_CONFIG_HOME/purgery/client.toml`
4. `$HOME/.config/purgery/client.toml`

Empty environment variables are ignored. The client has no `/etc` fallback.

## Server config

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

[gc]
incoming_lease_secs = 1800
heartbeat_interval_secs = 60

[logging]
level = "info"
format = "pretty"
color = "auto"
```

`work_dir` and every root `path` must be absolute. At least one root is required. Root names are unique, non-empty, and contain only ASCII alphanumeric characters, `-`, and `_`.

A postprocess step is server-owned. `program` is resolved by the server. `args` is an argv template, not a shell command. Supported placeholders are `{input}`, `{input_dir}`, `{file_name}`, `{file_stem}`, `{extension}`, and `{work_dir}`. `expected_outputs` are validated as safe relative paths and must remain inside the selected server root after resolution. Steps execute in the client's listed order.

## Client config

```toml
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"
command = "purgery-server"

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

[logging]
level = "info"
format = "pretty"
color = "auto"
```

`nickname` is operational metadata and contains only ASCII alphanumeric characters, `-`, and `_`. `state_dir` and client root paths must be absolute. At least one client root is required, and client root names follow the same uniqueness and character rules as server roots.

### Root-qualified sources

`from` has one of these forms:

```text
<client-root-name>
<client-root-name>/<path-under-client-root>
```

It is required, relative, and normalized. It cannot be empty, `.`, start with `./`, contain `..`, or contain empty components. Its first component must name a configured client root.

Given root `videos = /home/user/Videos`, `from = "videos"` resolves to `/home/user/Videos`, while `from = "videos/cats"` resolves to `/home/user/Videos/cats`.

### Root-qualified destinations

`to` has one of these forms:

```text
<server-root-name>
<server-root-name>/<path-under-server-root>
```

It obeys the same relative-path normalization rules. The server validates the first component against its configured roots and performs final path-within-root checks.

### Sync identity

Sync IDs are generated in config order:

```text
sync-0001
sync-0002
sync-0003
```

These IDs may occur in manifests, run files, statuses, and diagnostics. Logs also include `from` and `to`, which are the meaningful configuration identity.

### Match

`match` is optional. Without it, all entries beneath the resolved source are selected. With it, the existing rsync-style matcher is applied to normalized paths relative to that source. Regular files and symlinks must match. Ancestor directories required by selected descendants are included structurally.

Only selected entries may be uploaded or cleaned up. Structural directories may be removed only by safe bottom-up cleanup when empty.

### Postprocess selection

`postprocess` is optional. When present it must be a non-empty list of server-defined step names. A string is invalid, including for a single step. The server rejects unknown steps and unsafe expected outputs. The client never supplies executable definitions.

A sync with `postprocess` must set `delete_after_import = true`. Selected regular files carry SHA-256 identity. When `match` is also present, only matched entries are postprocessed; without `match`, every selected entry under the source is postprocessed.

### Cleanup

`delete_after_import` defaults to `false`. When true, only selected local entries confirmed as successfully imported may be removed. Unmatched files are not uploaded and cannot be deleted. Passthrough cleanup uses durable transfer identity; postprocessed cleanup uses terminal server status and source identity.

## Run config

The uploaded run config contains the nickname, generated sync IDs, root-qualified server destinations, and ordered postprocess selections needed by the server. Client root definitions and absolute client source paths are not required by server processing.

Status `final_paths` are root-qualified archive paths such as `univ/videos/dogs/dog-beach.jpeg`. They are never absolute and never contain the nickname.
