
Purgery orchestrates one-way file flows: send files to a remote location, optionally process them there, and optionally remove the local copies afterward.

## Problem

Files in one place sometimes also need to be in another place:

* Recordings from a device that should be stored on a remote drive.
* Output directories that should be delivered to a shared location.
* Staging areas that should be cleared after their contents have been sent elsewhere.

Some of these files can be copied as-is. Some should be stored in a compressed or generated form. Some sources should remain in place after sending. Some should be removed.

## What Purgery is for

A basic Purgery operation sends one filesystem entry — a file, directory, or symlink — to a destination according to a configured plan.

Depending on how you configure it:

* **Send as-is and keep the source** — the source entry is copied to the destination unchanged. The original remains in place.
* **Send as-is and remove the source** — the source entry is copied unchanged; after a successful transfer the original is removed.
* **Send processed output and remove the source** — processed output is placed at the destination; after confirmation the original is removed.

Examples:

* Store compressed versions of large recordings; remove the large originals after compression succeeds.
* Send project build outputs to a shared archive; remove the local build directory afterward.
* Send photos or videos from a laptop to long-term storage without removing them from the laptop.

## Basic usage

Send a file as-is and keep the source:

```
purgery-client sync -- ~/photo.jpeg user@server:/archive
```
Send a file, request remote processing, and remove the source:

```
purgery-client sync \
  --transform compress-video \
  --delete-after-import \
  -- ~/Videos/trip.mp4 user@server:/archive
```

Send a whole directory:

```
purgery-client sync \
  --transform compress-video \
  --delete-after-import \
  --split '**/*.mp4' \
  -- ~/Videos user@server:/archive
```

The above will match all video files in `~/Videos` and send them one-by-one, compressing on the server, and then deleting the originals.

## What Purgery is not

* **Not a backup system.** Purgery sends files in one direction. It does not track versions, maintain an index, or restore previous states.
* **Not bidirectional synchronization.** Flows are one-way. Changes at the destination are not propagated back to the source.
* **Not a live directory watcher.** Purgery runs on demand for a single source entry. It does not continuously monitor directories.
* **Not a general-purpose file organizer.** Every flow handles exactly one source entry. Multi-entry trees must be sent explicitly or via the `--split` pattern.
* **Not primarily a media conversion tool.** Processing is optional and server-defined. The tool does not ship built-in encoders or converters.

## More documentation

- [Config reference](docs/config.md) — server configuration, transform definitions, run configuration
- [Protocol](docs/protocol.md) — lifecycle, subcommands, run states, status format, version compatibility
- [Operations](docs/operations.md) — check, GC, heartbeat, leases, split patterns
- [Import semantics](docs/design/import-semantics.md) — one-source-entry model, work areas, per-entry rules
- [Crash safety and idempotence](docs/design/crash-safety-and-idempotence.md) — durable phases, replay recovery, atomic replacement, deletion authority

# License

Purgery is free software. Purgery is distributed under AGPL version 3.

