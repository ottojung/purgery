// ── Rsync Filter Generation ──────────────────────────────────────────

pub fn rsync_pattern_match(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }

    let anchored = pattern.starts_with('/');
    let pat_body = if anchored { &pattern[1..] } else { pattern };

    let pat_str = pat_body;

    fn match_rsync(pat: &[u8], path: &[u8]) -> bool {
        match (pat.first(), path.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(&b'*'), _) => {
                if pat.len() > 1 && pat[1] == b'*' {
                    let rest = &pat[2..];
                    if rest.is_empty() || (rest.len() == 1 && rest[0] == b'/') {
                        return true;
                    }
                    let rest = if rest[0] == b'/' { &rest[1..] } else { rest };
                    for i in 0..=path.len() {
                        if match_rsync(rest, &path[i..]) {
                            return true;
                        }
                    }
                    false
                } else {
                    let mut i = 0;
                    while i <= path.len() {
                        if i > 0 && path[i - 1] == b'/' {
                            break;
                        }
                        if match_rsync(&pat[1..], &path[i..]) {
                            return true;
                        }
                        i += 1;
                    }
                    false
                }
            }
            (Some(&b'?'), Some(_)) if path[0] != b'/' => match_rsync(&pat[1..], &path[1..]),
            (Some(&p), Some(&q)) if p == q => match_rsync(&pat[1..], &path[1..]),
            _ => false,
        }
    }

    let pat_bytes = pat_str.as_bytes();
    let path_bytes = path.as_bytes();

    if anchored {
        match_rsync(pat_bytes, path_bytes)
    } else {
        for start in 0..path_bytes.len() {
            if match_rsync(pat_bytes, &path_bytes[start..]) {
                return true;
            }
        }
        false
    }
}
