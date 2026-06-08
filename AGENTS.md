# Purgery — Agent Instructions

## 1. Project Overview

Purgery is a Rust client/server file sync and postprocessing tool. Two binaries:

```
purgery-client    # uploads files via rsync, deletes confirmed imports
purgery-server    # processes ready runs, postprocesses, writes status
```

Shared types live in `crates/purgery-core`.

## 2. Setup

```sh
cargo build --workspace
```

No external dependencies beyond Rust toolchain.

## 3. Commands

```sh
cargo fmt --all -- --check      # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo test --workspace          # run all tests
cargo build --workspace         # build everything
```

## 4. Rust Coding Conventions

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
| `PurgeryRoot` | Absolute path |
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

## 5. Testing

- Unit tests for config parsing, path validation, run status parsing, and deletion safety.
- Integration tests should avoid requiring real SSH initially.
- Tests live alongside code (`#[cfg(test)] mod tests`).

## 6. Compatibility

- Correctness and beauty must override backwards compatibility.

## Git is good

If you have access to `git`, then:
- commit frequently,
- and write helpful commit messages.

It is always safe to commit, do it even if you weren't explicitly told to.

## Github code of conduct

When posting a comment on github, include `@ottojung` substring at the end.
Github posts must use humor and metaphors.
