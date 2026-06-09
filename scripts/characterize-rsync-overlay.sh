#!/usr/bin/env bash
set -u

# rsync has no --no-delete switch: omission of every --delete option is the
# no-delete mode. Archive mode preserves symlinks and implies recursion.
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
show() { find "$1" -mindepth 1 -printf '%P|%y|' -exec sh -c 'p="$1"; if [ -L "$p" ]; then readlink "$p"; elif [ -f "$p" ]; then cat "$p"; fi' sh {} \; -printf '\n' | sort; }
case_run() {
  name=$1; setup=$2
  rm -rf "$root/src" "$root/dst"; mkdir -p "$root/src" "$root/dst"
  eval "$setup"
  set +e
  out=$(rsync --recursive --archive "$root/src/" "$root/dst/" 2>&1); rc=$?
  set -e
  printf '\n=== %s rc=%s ===\n%s\n' "$name" "$rc" "$out"
  show "$root/dst"
}
set -e
case_run 'directory over missing' 'mkdir -p "$root/src/x"'
case_run 'directory over directory' 'mkdir -p "$root/src/x" "$root/dst/x"; echo extra > "$root/dst/x/extra"'
case_run 'directory over file' 'mkdir -p "$root/src/x"; echo old > "$root/dst/x"'
case_run 'directory over symlink' 'mkdir -p "$root/src/x" "$root/dst/target"; ln -s target "$root/dst/x"'
case_run 'file over missing' 'echo new > "$root/src/x"'
case_run 'file over file' 'echo new-content > "$root/src/x"; echo old > "$root/dst/x"'
case_run 'file over symlink' 'echo new > "$root/src/x"; echo target > "$root/dst/target"; ln -s target "$root/dst/x"'
case_run 'file over empty dir' 'echo new > "$root/src/x"; mkdir -p "$root/dst/x"'
case_run 'file over nonempty dir' 'echo new > "$root/src/x"; mkdir -p "$root/dst/x"; echo extra > "$root/dst/x/extra"'
case_run 'symlink over missing' 'ln -s target "$root/src/x"'
case_run 'symlink over file' 'ln -s target "$root/src/x"; echo old > "$root/dst/x"'
case_run 'symlink over symlink' 'ln -s target "$root/src/x"; ln -s old "$root/dst/x"'
case_run 'symlink over empty dir' 'ln -s target "$root/src/x"; mkdir -p "$root/dst/x"'
case_run 'symlink over nonempty dir' 'ln -s target "$root/src/x"; mkdir -p "$root/dst/x"; echo extra > "$root/dst/x/extra"'
case_run 'child final parent symlink' 'mkdir -p "$root/src/p" "$root/dst/target"; echo new > "$root/src/p/c"; ln -s target "$root/dst/p"'
case_run 'child final parent file' 'mkdir -p "$root/src/p"; echo new > "$root/src/p/c"; echo old > "$root/dst/p"'
