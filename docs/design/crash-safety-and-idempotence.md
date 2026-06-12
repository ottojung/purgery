# Crash Safety and Idempotent Imports

## Safety state

Purgery persists deletion-authority state before cleanup can occur. Postprocess runs retain the uploaded run config, manifest, run ID, nickname, and client phase. Cleanup-enabled passthrough transfers retain selected source identity and transfer completion.

Writes use atomic replacement so a crash exposes either the previous valid state or the complete next state. Failure to persist required safety state stops the operation.

## Selection boundary

Cleanup authority is scoped to entries selected from a resolved root-qualified source. Optional match filtering is applied before transfer and before cleanup state is created. Unmatched files never enter deletion-authority state.

Generated sync IDs are deterministic within config order and connect manifests, run config, status, and diagnostics. The accompanying `from` and `to` values explain the user-facing mapping. Client roots remain client-side; server recovery uses uploaded run data only.

## Postprocess recovery

A postprocess entry is cleaned only after a verified terminal server status reports successful import and the local source identity still matches. Regular files use SHA-256 identity. Unknown steps, unsafe outputs, missing outputs, or failed commands leave the original in place.

The server persists phase and progress state around processing. Recovery can replay work from staged inputs, verify already committed outputs, and continue without placing the nickname in archive paths.

## Passthrough recovery

Cleanup-enabled passthrough imports record pre-transfer identity, successful transfer completion, and cleanup progress. A restarted client can continue cleanup only for those selected entries. A source that changed after transfer is retained.

Passthrough imports without cleanup need no deletion state.

## Directory safety

Selected files and symlinks are considered individually. Structural ancestor directories do not independently authorize deletion. After eligible children are removed, directories may be attempted bottom-up and removed only when empty and path-safe. Unmatched descendants therefore keep their ancestors present.

## Idempotent overlay

Imports overlay selected paths into an existing archive tree. Replaying an entry does not delete unrelated archive content. Existing outputs are accepted only after identity and containment checks. Conflicting or unverifiable outputs fail safely.

Final-path construction always uses the selected named server root, the path under that root, and the selected entry or validated output path. The nickname namespaces operational state only.

## Failure matrix

| Failure point | Required result |
|---|---|
| Before transfer success is persisted | No passthrough deletion |
| After transfer success, before cleanup | Resume identity-checked cleanup |
| Before a postprocess run reaches terminal status | No postprocess deletion |
| Postprocess entry fails | Keep that local original |
| Terminal status is corrupt or inconsistent | Block cleanup |
| Local source identity changed | Keep the changed entry |
| Match excludes an entry | Never upload or delete it |
| Required safety-state write fails | Stop and report operator action |
| Run state is abandoned or corrupt | Tombstone/block automatic continuation |

## Leases and progress

Incoming leases prevent garbage collection of active uploads. Processing progress heartbeats identify live work. Expired incoming runs may be collected according to server policy, while ready, processing, completed, and tombstoned state follows phase-specific recovery rules.
