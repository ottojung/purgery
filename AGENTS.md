# Purgery — Agent Instructions

## Project Overview

Purgery is a Rust client/server file sync and transforming tool. Two binaries:

```
purgery-client    # uploads files via rsync, deletes confirmed imports
purgery-server    # processes ready runs, transformes, writes status
```

Shared types live in `crates/purgery-core`.

## Setup

```sh
cargo build --workspace
```

No external dependencies beyond Rust toolchain.

## Commands

```sh
cargo fmt --all -- --check      # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo test --workspace          # run all tests
cargo build --workspace         # build everything
```

## Rust Coding Conventions

### Side-effect boundaries

Filesystem, command execution, environment access, and logging should be routed through narrow interfaces where practical. Do not scatter raw `std::fs`, `std::process::Command`, or `std::env` across domain logic — keep them behind capability-like function arguments or wrapper types.

### Boundary validation

TOML configs, manifests, status files, CLI args, SSH paths, and filesystem paths are **boundary data**. Parse them into validated internal types before use. Raw strings / unvalidated paths must not reach domain code.

### Invariant-carrying types

Use newtypes for values that carry invariants:

| Type | Invariant |
|------|-----------|
| `Nickname` | Non-empty, alphanumeric + `-` `_` only |
| `RunId` | Non-empty, [A-Za-z0-9._-] |
| `ServerRoot` | Absolute path |
| `ServerWorkDir` | Absolute path |
| `RelativeDestinationPath` | Relative, no `..`, no empty components |
| `NormalizedRelativePath` | Relative, no `..`, no empty components |

Each newtype documents:
- what properties it carries
- where those properties are established (proof in comment)

### Error handling

- Use specific `thiserror` enums in library code.
- Use `anyhow` only near binary/application boundaries.
- Keep errors local and inspectable; prefer `Result<T, E>` over panics.

### State modelling

- Prefer enums and separate types over boolean flags.
- Make impossible states unrepresentable where practical.

## Testing

- Unit tests for config parsing, path validation, run status parsing, and deletion safety.
- Integration tests should avoid requiring real SSH initially.
- Tests live alongside code (`#[cfg(test)] mod tests`).

## Compatibility

- Correctness and beauty must override backwards compatibility.

## Don't speak of the dead

Comments, JSDoc, tests, and documentation must describe the codebase as it exists.

Do not bring the conversation, prompt, development process, previous implementation, or discarded design into the source tree. The reader should not need to know what the agent was asked, what the agent changed, what used to be here, or why the new version is “better”. That belongs in the issue, pull request, commit message, or changelog.

Only the current codebase exists. Everything outside it is a postapocalyptic no-man's land: old code, abandoned plans, prompt context, temporary reasoning, and implementation attempts. Nothing living can survive there. Do not smuggle it back into the repository.

Bad:

```js
// Previously this accepted raw strings, but now it uses EventId.
```

Bad:

```js
// This improved version avoids the old race condition.
```

Bad:

```js
// The user asked for this to be stricter.
```

Bad:

```js
// Refactored during the AGENTS.md cleanup.
```

Good:

```js
// EventId prevents persisted event identifiers from being confused with arbitrary strings.
```

Good:

```js
// Writes are serialized through the repository lock so concurrent callers cannot commit conflicting updates.
```

Good:

```js
// The parser rejects unknown fields because client and server schemas may drift during development.
```

The rule is:

> Replace history with invariants. Replace process narration with current design. Replace comparison against dead code with explanation of living code.

Historical context is allowed only when the history is itself part of the live external contract: migrations, persisted file formats, compatibility boundaries, or public APIs.

Good:

```js
// Version 1 records do not contain `createdAt`, so the migration derives it from file metadata.
```

Bad:

```js
// Before this change, records did not contain `createdAt`.
```

Git remembers the dead. Source comments serve the living.

## Git is good

If you have access to `git`, then:
- commit frequently,
- and write helpful commit messages.

It is always safe to commit, do it even if you weren't explicitly told to.

## Github code of conduct

When posting a comment on github, include `@ottojung` substring at the end.
Github posts must use humor and metaphors.
