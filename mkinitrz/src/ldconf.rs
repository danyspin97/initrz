//! Parses /etc/ld.so.conf (and any files it `include`s) to find extra shared library search
//! paths - e.g. multiarch paths like Debian's /usr/lib/x86_64-linux-gnu that aren't in any
//! standard lib64/lib layout.

use std::collections::HashSet;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};

const LD_SO_CONF: &str = "/etc/ld.so.conf";

/// Returns the directories listed in /etc/ld.so.conf and its includes, in file order. Silently
/// returns an empty list if /etc/ld.so.conf doesn't exist (e.g. non-glibc systems) - callers are
/// expected to fall back to a hardcoded default list in that case.
pub fn search_paths() -> Vec<Utf8PathBuf> {
    search_paths_from(Utf8Path::new(LD_SO_CONF))
}

/// Same as `search_paths()`, but reads from an arbitrary conf file - split out for testing.
pub fn search_paths_from(conf: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    read_conf(conf, &mut paths, &mut seen);
    paths
}

fn read_conf(path: &Utf8Path, paths: &mut Vec<Utf8PathBuf>, seen: &mut HashSet<Utf8PathBuf>) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    // Relative include patterns resolve against the directory containing this conf file,
    // matching ldconfig's own behavior.
    let base_dir = path.parent().unwrap_or(Utf8Path::new("/"));

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(pattern) = line.strip_prefix("include ") {
            for included in glob_conf_pattern(pattern.trim(), base_dir) {
                read_conf(&included, paths, seen);
            }
            continue;
        }

        let dir = Utf8PathBuf::from(line);
        if seen.insert(dir.clone()) {
            paths.push(dir);
        }
    }
}

fn glob_conf_pattern(pattern: &str, base_dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let pattern = if Utf8Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        base_dir.join(pattern).to_string()
    };

    let Ok(entries) = glob::glob(&pattern) else {
        return Vec::new();
    };

    let mut matched: Vec<Utf8PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|p| Utf8PathBuf::from_path_buf(p).ok())
        .collect();
    matched.sort();
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_paths_with_include() {
        let conf = Utf8Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/ldconf/ld.so.conf"
        ));
        let paths = search_paths_from(conf);
        assert_eq!(
            paths,
            vec![
                Utf8PathBuf::from("/usr/local/lib64"),
                Utf8PathBuf::from("/usr/local/lib"),
                Utf8PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            ]
        );
    }

    #[test]
    fn test_search_paths_missing_file_returns_empty() {
        assert!(search_paths_from(Utf8Path::new("/nonexistent/ld.so.conf")).is_empty());
    }
}
