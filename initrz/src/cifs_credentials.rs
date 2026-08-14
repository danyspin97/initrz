//! Expands a `credentials=<file>` CIFS mount option into `username=`/`password=`/`domain=`
//! options read from that file, so a CIFS root's password doesn't have to sit in plain text on
//! the kernel cmdline (visible via /proc/cmdline to anything that can read it).
//!
//! The kernel's cifs.ko has no notion of `credentials=` itself - only the userspace mount.cifs
//! tool translates it, by reading the file and passing its keys straight through as mount
//! options. Since initrz never calls mount.cifs, that translation has to happen here instead.

use std::fs;

use anyhow::{Context, Result};

/// If `options` contains `credentials=<file>`, reads that file (mount.cifs credentials format:
/// `key=value` lines, blank lines and `#` comments ignored) and returns `options` with the
/// `credentials=` entry replaced by the file's keys - except for any key already present
/// explicitly in `options`, which takes precedence over the file. A no-op if there's no
/// `credentials=` option.
pub fn resolve(options: &[String]) -> Result<Vec<String>> {
    let Some(pos) = options.iter().position(|o| o.starts_with("credentials=")) else {
        return Ok(options.to_vec());
    };

    let path = &options[pos]["credentials=".len()..];
    let content = fs::read_to_string(path)
        .with_context(|| format!("unable to read CIFS credentials file {:?}", path))?;

    let mut resolved: Vec<String> = options
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != pos)
        .map(|(_, o)| o.clone())
        .collect();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key_prefix = format!("{}=", key);
        if !resolved.iter().any(|o| o.starts_with(&key_prefix)) {
            resolved.push(line.to_string());
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_credentials_option_is_noop() {
        let options = vec!["vers=3.0".to_string()];
        assert_eq!(resolve(&options).unwrap(), options);
    }

    #[test]
    fn test_expands_credentials_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/cifs-credentials");
        let options = vec!["vers=3.0".to_string(), format!("credentials={}", path)];
        let resolved = resolve(&options).unwrap();

        assert_eq!(resolved.len(), 4);
        assert_eq!(resolved[0], "vers=3.0");
        assert!(resolved.contains(&"username=alice".to_string()));
        assert!(resolved.contains(&"password=hunter2".to_string()));
        assert!(resolved.contains(&"domain=EXAMPLE".to_string()));
        assert!(!resolved.iter().any(|o| o.starts_with("credentials=")));
    }

    #[test]
    fn test_explicit_option_overrides_credentials_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/cifs-credentials");
        let options = vec![format!("credentials={}", path), "username=bob".to_string()];
        let resolved = resolve(&options).unwrap();

        assert!(resolved.contains(&"username=bob".to_string()));
        assert!(!resolved.contains(&"username=alice".to_string()));
    }

    #[test]
    fn test_missing_credentials_file_errors() {
        let options = vec!["credentials=/nonexistent/creds".to_string()];
        assert!(resolve(&options).is_err());
    }
}
