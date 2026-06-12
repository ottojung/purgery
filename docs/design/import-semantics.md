# Import Semantics

## Root-qualified mapping

Every sync connects a named client root to a named server root:

```text
from = "<client-root-name>/<path-under-client-root>"
to   = "<server-root-name>/<path-under-server-root>"
```

The client resolves `from`; the server resolves `to`. The server never needs client root definitions. For a selected entry, the path relative to the resolved source is preserved beneath the resolved destination.

```text
client root videos = /home/user/Videos
from = videos/cats
to = univ/videos/cats
entry = kitten-01.mp4

local: /home/user/Videos/cats/kitten-01.mp4
final: /universe/synced/videos/cats/kitten-01.mp4
status: univ/videos/cats/kitten-01.mp4
```

The nickname is operational metadata and is absent from final paths.

## Selection

The resolved source is the selection boundary. Relative manifest paths are computed beneath it.

Without `match`, every encountered entry is selected. With `match`, Purgery applies its rsync-style pattern matcher to normalized relative paths. Matching regular files and symlinks are selected. Directories required to materialize selected descendants are structural entries.

Only selected entries are uploaded and eligible for cleanup. Unmatched entries remain outside the import. Structural ancestors may be removed locally only by safe bottom-up cleanup when empty.

## Entry modes

### Passthrough

A selected entry without a `postprocess` list is copied directly to the destination. `delete_after_import` defaults to false. When cleanup is enabled, durable source identity and successful transfer state authorize deletion.

### Postprocess

A selected entry with `postprocess = ["step-a", "step-b"]` is staged for server processing. The ordered names are attached to the manifest entry. The server resolves them against server-owned definitions and runs them in order.

Postprocess syncs require `delete_after_import = true`, and postprocessed regular files carry SHA-256 source identity. A failed transform or unsafe output produces no cleanup authority.

### Structural or covered entries

Directories required by selected descendants establish tree shape. Coverage metadata prevents a structural entry from independently acting as an imported source object. Directory cleanup remains bottom-up and emptiness-checked.

## Storage invariants

1. Archive roots are configured only on the server.
2. Every final path is contained by the selected server root.
3. Existing unrelated archive entries are preserved.
4. Imports overlay selected entries; they do not implicitly delete archive content.
5. Status `final_paths` are root-qualified relative paths.
6. Neither absolute server paths nor the client nickname appear in `final_paths`.

## Postprocess outputs

Expected output names are part of server step definitions. The server validates templates, expands them in an isolated work area, checks output existence and type, and resolves each output beneath the sync destination. `keep_original` is also server-owned.

For `dog-beach.png` selected by `videos -> univ/videos`, a server step whose expected output is `{file_stem}.jpeg` may report:

```text
univ/videos/dogs/dog-beach.jpeg
```

Output paths are plural because a step may materialize multiple validated outputs.

## Commit and replay

Passthrough imports copy selected entries directly or commit staged passthrough entries, depending on the run path. Postprocess imports prepare isolated work areas, execute steps, and commit validated outputs. Directory commits use overlay semantics.

Durable phase and progress state makes processing replayable. A replay verifies already materialized outputs before reporting success. Per-entry failures do not roll back entries already committed by the same run.

## Cleanup invariant

A local entry may be deleted only when all of these hold:

- the sync enables cleanup;
- the entry was selected by that sync;
- successful import is confirmed by the applicable authority;
- recorded identity still matches the local object;
- path and entry-kind safety checks pass.

For a dog-image match, unrelated files such as `dogs/readme.txt` and `trips/baku-arrival.mp4` are not selected, not uploaded, and not deleted.
