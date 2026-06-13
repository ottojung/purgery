/// Rsync-style pattern matching for --split.
use std::path::Path;
use walkdir::WalkDir;

pub(crate) struct SplitCandidate {
    pub path: String,
    pub is_dir: bool,
}

/// Discover split entries under `source` that match `pattern`.
///
/// Returns a deterministic non-overlapping set of root paths in
/// normalized path order, with ancestor pruning applied.
pub(crate) fn discover_split_entries(
    source: &str,
    pattern: &str,
) -> Result<Vec<SplitCandidate>, String> {
    let source_path = Path::new(source);
    if std::fs::symlink_metadata(source_path).is_err() {
        return Ok(Vec::new());
    }

    let matcher = PatternMatcher::new(pattern);

    // Collect all candidates (including source itself).
    let mut candidates: Vec<SplitCandidate> = Vec::new();

    // SOURCE itself is candidate ".".
    candidates.push(SplitCandidate {
        path: source.to_owned(),
        is_dir: source_path.is_dir(),
    });

    // Walk descendants.
    for walk_entry in WalkDir::new(source_path).follow_links(false).min_depth(1) {
        let walk_entry = match walk_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = walk_entry.path();
        let relative = match path.strip_prefix(source_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let candidate = source_path.join(relative);
        candidates.push(SplitCandidate {
            path: candidate.to_string_lossy().to_string(),
            is_dir: walk_entry.file_type().is_dir(),
        });
    }

    // Filter by pattern.
    let matched: Vec<SplitCandidate> = candidates
        .into_iter()
        .filter(|c| {
            let relative_str = if c.path == source {
                ".".to_string()
            } else {
                let path = Path::new(&c.path);
                path.strip_prefix(source_path)
                    .unwrap_or(std::path::Path::new(""))
                    .to_string_lossy()
                    .to_string()
            };
            matcher.is_match(&relative_str, c.is_dir)
        })
        .collect();

    // Prune descendants of matched ancestors.
    let mut pruned: Vec<SplitCandidate> = Vec::new();
    for candidate in &matched {
        if candidate.path == source {
            // SOURCE matches — all descendants are pruned.
            return Ok(vec![SplitCandidate {
                path: source.to_owned(),
                is_dir: true,
            }]);
        }
        // Check if any ancestor is also matched.
        let has_ancestor = matched.iter().any(|other| {
            if other.path == candidate.path {
                return false;
            }
            let candidate_path = Path::new(&candidate.path);
            let other_path = Path::new(&other.path);
            candidate_path.starts_with(other_path)
        });
        if !has_ancestor {
            pruned.push(SplitCandidate {
                path: candidate.path.clone(),
                is_dir: candidate.is_dir,
            });
        }
    }

    // Deterministic sort.
    pruned.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(pruned)
}

/// Compute the target suffix for a matched root relative to source.
///
/// - source itself → empty (root already matches target)
/// - top-level child → "/" (rsync destination parent)
/// - nested child → "/parent" (no trailing slash — run_rsync adds it)
pub(crate) fn split_target_suffix(source: &str, root: &str) -> String {
    if root == source {
        return String::new();
    }
    let source_path = Path::new(source);
    let root_path = Path::new(root);
    if let Ok(relative) = root_path.strip_prefix(source_path) {
        let parent = relative
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""));
        if parent.as_os_str().is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parent.to_string_lossy())
        }
    } else {
        String::new()
    }
}

struct PatternMatcher {
    pattern: String,
    anchored: bool,
    dir_only: bool,
}

impl PatternMatcher {
    fn new(pattern: &str) -> Self {
        let mut p = pattern;
        let anchored = p.starts_with('/');
        if anchored {
            p = &p[1..];
        }
        let dir_only = p.ends_with('/');
        if dir_only {
            p = &p[..p.len() - 1];
        }
        PatternMatcher {
            pattern: p.to_owned(),
            anchored,
            dir_only,
        }
    }

    fn is_match(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        // The root sentinel "." is not a regular path component and
        // should not match dir-only patterns like "*/".
        if self.dir_only && path == "." {
            return false;
        }

        // For rsync patterns without /, match against any component.
        let has_slash = self.pattern.contains('/') || self.anchored;

        if has_slash || self.anchored {
            glob_match(&self.pattern, path)
        } else {
            // Unanchored pattern without /: match against any component.
            let components: Vec<&str> = path.split('/').collect();
            components.iter().any(|c| glob_match(&self.pattern, c))
        }
    }
}

/// Simple glob matching supporting * and **.
/// - * matches anything except /
/// - ** matches anything including /
fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern == "**" {
        return true;
    }

    if !pattern.contains("**") {
        return simple_glob(pattern, path);
    }

    // Split by **. Each part is itself a simple glob pattern.
    let parts: Vec<&str> = pattern.split("**").collect();

    // Strip leading / from parts after **, since ** matches any depth.
    let segments: Vec<&str> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if p.is_empty() {
                p
            } else if i > 0 && p.starts_with('/') {
                &p[1..]
            } else {
                p
            }
        })
        .collect();

    let parts_with_positions: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, _)| i)
        .collect();

    if parts_with_positions.is_empty() {
        return true; // just **
    }

    let _first_idx = parts_with_positions[0];
    let _last_idx = parts_with_positions[parts_with_positions.len() - 1];

    let mut search_start = 0usize;

    for (i, &seg_idx) in parts_with_positions.iter().enumerate() {
        let segment = segments[seg_idx];
        if i == 0 && seg_idx == 0 {
            // First part matches at start.
            if !simple_glob(segment, path) {
                return false;
            }
            if let Some(len) = first_match_len(segment, path, 0) {
                search_start = len;
            } else {
                return false;
            }
        } else if i == parts_with_positions.len() - 1 && seg_idx == segments.len() - 1 {
            // Last part after ** matches a suffix of the path anchored at end.
            // Try each suffix starting after a / boundary, from shortest to longest.
            let mut suffix_start = match path[search_start..].rfind('/') {
                Some(pos) => search_start + pos + 1,
                None => search_start,
            };
            let mut found_suffix = false;
            loop {
                let suffix = &path[suffix_start..];
                if simple_glob(segment, suffix) {
                    found_suffix = true;
                    break;
                }
                if suffix_start <= search_start {
                    break;
                }
                // Move to previous / boundary
                match path[..suffix_start - 1].rfind('/') {
                    Some(pos) => suffix_start = pos + 1,
                    None => {
                        suffix_start = search_start;
                    }
                }
            }
            if !found_suffix {
                return false;
            }
        } else {
            // Middle part matches somewhere after search_start.
            let mut found = false;
            let mut pos = search_start;
            while pos <= path.len() {
                if simple_glob(segment, &path[pos..]) {
                    if let Some(len) = first_match_len(segment, &path[pos..], 0) {
                        search_start = pos + len;
                        found = true;
                        break;
                    }
                }
                pos += 1;
            }
            if !found {
                return false;
            }
        }
    }
    true
}

fn first_match_len(pattern: &str, path: &str, start: usize) -> Option<usize> {
    let mut pi = 0;
    let mut si = start;
    let pat = pattern.as_bytes();
    let pth = path.as_bytes();

    while pi < pat.len() {
        match pat[pi] {
            b'*' => {
                pi += 1;
                if pi == pat.len() {
                    // Match rest of path up to /
                    let end = path[si..].find('/').unwrap_or(path.len() - si);
                    return Some(si + end);
                }
                let mut matched = None;
                while si < pth.len() && pth[si] != b'/' {
                    if let Some(len) = first_match_len(&pattern[pi..], path, si) {
                        matched = Some(len);
                        break;
                    }
                    si += 1;
                }
                return matched;
            }
            b'?' => {
                if si >= pth.len() || pth[si] == b'/' {
                    return None;
                }
                pi += 1;
                si += 1;
            }
            c => {
                if si >= pth.len() || pth[si] != c {
                    return None;
                }
                pi += 1;
                si += 1;
            }
        }
    }
    Some(si)
}

fn simple_glob(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == path;
    }

    let pat_bytes = pattern.as_bytes();
    let path_bytes = path.as_bytes();
    let mut pi = 0;
    let mut si = 0;

    while pi < pat_bytes.len() {
        match pat_bytes[pi] {
            b'*' => {
                // * matches any chars except /
                pi += 1;
                if pi == pat_bytes.len() {
                    return !path[si..].contains('/');
                }
                // Try to match the rest at each position (including at a / boundary)
                while si < path_bytes.len() {
                    if simple_glob(&pattern[pi..], &path[si..]) {
                        return true;
                    }
                    if path_bytes[si] == b'/' {
                        break;
                    }
                    si += 1;
                }
                return false;
            }
            b'?' => {
                if si >= path_bytes.len() || path_bytes[si] == b'/' {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            c => {
                if si >= path_bytes.len() || path_bytes[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }

    si == path_bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn pattern_star_matches_single_component() {
        let m = PatternMatcher::new("*.txt");
        assert!(m.is_match("file.txt", false));
        assert!(m.is_match("sub/file.txt", false));
    }

    #[test]
    fn pattern_star_does_not_cross_slash() {
        let m = PatternMatcher::new("*/*.txt");
        assert!(m.is_match("sub/file.txt", false));
        assert!(!m.is_match("file.txt", false));
    }

    #[test]
    fn pattern_doublestar_matches_across_dirs() {
        let m = PatternMatcher::new("**/*.txt");
        assert!(m.is_match("file.txt", false));
        assert!(m.is_match("sub/file.txt", false));
        assert!(m.is_match("a/b/c/file.txt", false));
    }

    #[test]
    fn pattern_anchored_matches_from_root() {
        let m = PatternMatcher::new("/file.txt");
        assert!(m.is_match("file.txt", false));
        assert!(!m.is_match("sub/file.txt", false));
    }

    #[test]
    fn discover_includes_source_itself() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "").unwrap();
        let entries = discover_split_entries(tmp.path().to_str().unwrap(), ".").unwrap();
        assert!(!entries.is_empty());
        // Should include the source directory itself when "." matches
    }

    #[test]
    fn ancestor_pruning_removes_descendants() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.jpg"), "").unwrap();
        let entries = discover_split_entries(tmp.path().to_str().unwrap(), "**").unwrap();
        // ** matches everything, so only source itself should remain
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn top_level_child_has_slash_suffix() {
        let suffix = split_target_suffix("/src", "/src/a.mp4");
        assert_eq!(suffix, "/");
    }

    #[test]
    fn nested_child_has_parent_suffix() {
        let suffix = split_target_suffix("/src", "/src/2024/a.mp4");
        assert_eq!(suffix, "/2024");
    }

    #[test]
    fn source_itself_has_empty_suffix() {
        let suffix = split_target_suffix("/src", "/src");
        assert_eq!(suffix, "");
    }

    #[test]
    fn no_match_returns_empty() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "").unwrap();
        let entries = discover_split_entries(tmp.path().to_str().unwrap(), "*.mp4").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn split_dir_pattern_selects_directories() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.jpg"), "").unwrap();
        fs::write(tmp.path().join("readme.txt"), "").unwrap();
        // Pattern "*/" should select photos directory but not readme.txt
        let entries = discover_split_entries(tmp.path().to_str().unwrap(), "*/").unwrap();
        assert_eq!(entries.len(), 1);
        let path = std::path::Path::new(&entries[0].path);
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "photos");
    }
}
