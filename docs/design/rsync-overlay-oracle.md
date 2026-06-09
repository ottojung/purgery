# Rsync Overlay Conflict Oracle

Purgery's destination type-conflict rules are based on a local characterization of:

```text
rsync 3.2.7, protocol 31
rsync --recursive --archive SRC/ DST/
```

No `--delete*` option is supplied. Rsync has no `--no-delete` switch; omitting delete options is its no-delete mode.

The destination trees below use `d` for a directory, `f(content)` for a regular file and its content, and `l(target)` for a symlink and its literal target.

| Present source entry | Initial destination | Exit | Resulting relevant destination | Purgery rule |
|---|---|---:|---|---|
| directory `x/` | missing | 0 | `x/ d` | Create directory. |
| directory `x/` | directory containing `extra` | 0 | `x/ d`, `x/extra f(extra)` | Merge and retain unmentioned descendants. |
| directory `x/` | `x f(old)` | 0 | `x/ d` | Replace file with directory. |
| directory `x/` | `x l(target)` | 0 | `x/ d`; unrelated `target/` remains | Replace symlink itself; do not traverse it. |
| file `x f(new)` | missing | 0 | `x f(new)` | Create file. |
| file `x f(new-content)` | `x f(old)` | 0 | `x f(new-content)` | Replace file. |
| file `x f(new)` | `x l(target)` | 0 | `x f(new)`; target remains unchanged | Replace symlink itself; do not write through it. |
| file `x f(new)` | empty directory `x/` | 0 | `x f(new)` | Remove empty directory and create file. |
| file `x f(new)` | non-empty `x/extra` | 23 | directory and `extra` remain | Fail entry rather than delete descendants. |
| symlink `x l(target)` | missing | 0 | `x l(target)` | Create symlink with literal target. |
| symlink `x l(target)` | `x f(old)` | 0 | `x l(target)` | Replace file with symlink. |
| symlink `x l(target)` | `x l(old)` | 0 | `x l(target)` | Replace symlink target data. |
| symlink `x l(target)` | empty directory `x/` | 0 | `x l(target)` | Remove empty directory and create symlink. |
| symlink `x l(target)` | non-empty `x/extra` | 23 | directory and `extra` remain | Fail entry rather than delete descendants. |
| child `p/c f(new)` | parent `p l(target)` | 0 | `p/ d`, `p/c f(new)`; target remains | Parent directory entry replaces symlink before child import. |
| child `p/c f(new)` | parent `p f(old)` | 0 | `p/ d`, `p/c f(new)` | Parent directory entry replaces file before child import. |

## Purgery run-level difference

Rsync applies multiple source invocations in command order. Purgery does not define a run as an ordered overlay language. If two manifest entries resolve to the same final path—even if both are directories—the server rejects the run before importing any entry. Distinct final paths remain valid. This makes overlapping sync mappings explicit instead of allowing manifest sorting to choose a winner.

Purgery also requires generated postprocess outputs to be real regular files according to `symlink_metadata`. A symlink, directory, FIFO, socket, device, or missing expected output fails that input entry and is never followed or copied as a regular file.
