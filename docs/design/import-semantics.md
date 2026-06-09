# Import Semantics

## Recursive no-delete tree overlay

Purgery imports uploaded filesystem trees, not only regular files. For each sync mapping, the server overlays the staged tree onto `<root>/<nickname>/<sync.to>` with the same type-conflict behavior observed from `rsync --recursive --archive` when no delete option is supplied:

- directories are created or merged recursively;
- regular files are created or replaced;
- symlinks are preserved as symlinks, and their target strings are treated as literal data;
- final entries absent from the upload remain untouched;
- final-storage symlinks are never followed as directories.

The checked-in [`scripts/characterize-rsync-overlay.sh`](../../scripts/characterize-rsync-overlay.sh) script records the conflict oracle. With rsync 3.2.7, directories replace files and symlinks, files and symlinks replace files, symlinks, and empty directories, and files or symlinks fail rather than replace non-empty directories. A source directory resolves a conflicting file or symlink parent before its children are imported. Purgery follows those rules.

There is no `--no-delete` rsync option: no-delete is rsync's behavior when none of the `--delete*` options are supplied. Purgery never adds a delete option.

## Manifest order and validation

The manifest contains `directory`, `regular_file`, and `symlink` entries. Entries are ordered by depth, with parent directories before children and stable lexical ordering within depth/type groups. Empty directories therefore participate in imports and parent type conflicts are resolved before child entries.

The server requires every `staged_path` to equal the path derived from the run configuration and relative path. It uses `symlink_metadata` so staged symlinks are not followed. Regular files are checked by size and optional SHA-256, directories must be real directories, and symlink targets must exactly match the manifest's literal `link_target`.

Special filesystem objects are rejected explicitly by the client manifest builder.

## Per-entry commits

Directory commits create or retain a real directory and replace conflicting non-directories. Regular files are copied to a same-directory `.purgery-commit...tmp` file and renamed into place. Symlinks are created at a same-directory temporary name and renamed into place. Existing non-empty directories block a present source file or symlink, matching the characterized rsync behavior.

Every existing final-path ancestor must be a real directory. Purgery does not traverse final-storage symlinks. Source directory entries are responsible for replacing parent conflicts before descendants are processed.

No operation removes an unrelated descendant merely because it is absent from the upload. An existing destination directory is merged, so extra final files survive.

## Postprocessing

Postprocessing applies only to regular-file manifest entries. Directories and symlinks are imported directly and never passed to subprocesses. A regular staged file is copied into `<root>/.purgery-work/<nickname>/<run_id>/<sync.to>/<relative_path>`, matching rules run there, and each generated regular-file output is committed through the same final-tree regular-file mechanism.

`expected_outputs` are file-name templates only. Generated outputs must be regular files and remain in the input's work-area directory. `final_paths` is plural because one regular input may produce multiple outputs; directory and symlink status entries contain their single final path.

## Status and cleanup

Each status record includes the manifest entry kind and an `imported`, `failed`, or `skipped` result. The server continues after per-entry failures and publishes terminal success only after all entry operations have completed.

Client cleanup deletes only unchanged local regular files whose imported status and run envelope are valid and whose sync mapping enables `delete_after_import`. Local directories and symlinks are not deleted by cleanup.
