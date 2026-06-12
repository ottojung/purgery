# Purgery Protocol

## Boundary objects

The protocol exchanges validated run configuration, manifests, leases, progress, and terminal status. Boundary strings are parsed into invariant-carrying types before use.

A client config is reduced to server-relevant run data. Client roots and absolute client source paths remain client-side. Each sync is represented by a deterministic ID (`sync-0001`, `sync-0002`, ...), a root-qualified server destination, and its ordered postprocess step selection.

## Lifecycle

### Passthrough-only invocation

1. The client resolves each `from` through its configured client roots.
2. It walks the resolved source and applies optional `match` filtering.
3. It asks the server to resolve each root-qualified `to`.
4. The server validates root names and path containment and returns absolute rsync destinations.
5. The client transfers only selected entries.
6. Cleanup-enabled groups persist source identity and transfer success before safe local deletion.

### Invocation containing postprocess groups

1. The client generates a run ID and deterministic sync IDs.
2. It classifies selected entries. Postprocess entries carry ordered server step names and SHA-256 identity.
3. It uploads run config and manifest into an incoming run directory keyed by nickname and run ID.
4. `prepare-run` validates the envelope, destinations, manifest, selected step names, output templates, and transfer plan.
5. The client stages selected postprocess entries and performs any direct passthrough transfers.
6. `finish-run` makes the staged run ready.
7. The server processes ready entries, executes selected steps in order, and commits outputs under resolved server roots.
8. The server writes terminal status with root-qualified `final_paths`.
9. The client deletes only selected originals whose successful import and identity are confirmed.

The nickname namespaces operational run directories. It is absent from direct destinations and final archive paths.

## Selection and manifest entries

Paths used for matching and `relative_path` are normalized relative to the resolved sync source. With no `match`, all entries are selected. With `match`, matching regular files and symlinks are selected and required ancestor directories are included structurally. Unmatched entries do not appear in the transfer set and cannot authorize cleanup.

Manifest entries contain:

- generated sync ID;
- absolute validated local path for client cleanup identity and diagnostics;
- staged path;
- normalized path relative to the resolved sync source;
- entry kind and metadata;
- SHA-256 where cleanup or postprocessing requires identity;
- ordered postprocess step names for transformed entries;
- coverage metadata for structural directory handling.

Postprocess step names come from the sync selection. The server resolves them against its own step definitions. Unknown names fail planning.

## Destination resolution

A destination has the form:

```text
<server-root-name>
<server-root-name>/<path-under-root>
```

The server resolves the root name, joins the optional path under that root, then joins the manifest entry's relative path or validated postprocess output path. Every result must stay within the selected root.

Examples:

```text
univ/videos/cats/kitten-01.mp4
sys/server-configs/nginx/site.conf
univ/videos/dogs/dog-beach.jpeg
```

Status paths are root-qualified relative paths. They are not absolute and do not contain the client nickname.

## Entry modes

- **Passthrough:** selected entry is copied without transformation.
- **Postprocess:** selected entry is staged and transformed by the listed server-owned steps.
- **Covered/structural:** entry exists to materialize or represent a selected descendant and cannot independently authorize cleanup.

A sync with postprocessing must enable `delete_after_import`. Passthrough syncs default to retaining originals.

## Server validation

`prepare-run` rejects:

- envelope nickname or run-ID mismatches;
- unknown sync IDs;
- unknown server roots;
- destination or relative-path escapes;
- unknown postprocess step names;
- unsafe expected outputs;
- postprocess files without required SHA-256 identity;
- inconsistent entry modes or transfer paths.

Server processing depends only on uploaded run config and manifest data plus server configuration. It does not receive or resolve client roots.

## Status and cleanup authority

Terminal status records one result per manifest entry, including imported state, errors, and zero or more `final_paths`. Successful postprocess output paths are root-qualified. A failed entry authorizes no deletion.

The client verifies terminal status against durable local run state and source identity. For match-filtered syncs, only matching selected entries can be removed. Supporting directories are considered later by safe bottom-up empty-directory cleanup.

## Phases and recovery

Runs advance through incoming, ready, processing, done, or tombstoned phases. Durable phase and progress files make processing replayable. Existing committed outputs are verified before being treated as successful replay results. Progress heartbeat timestamps support lease-aware recovery and garbage collection.

Safety-state persistence is mandatory before any action that could lead to deletion. Corrupt or abandoned state blocks automatic cleanup and requires operator intervention.

## Subprocess safety

The server expands validated placeholders into explicit argv and executes the configured program without a shell. Work areas isolate staged inputs from final archive trees. Expected outputs are checked before commit, and final materialization revalidates containment under the selected server root.
