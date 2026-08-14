use std::convert::TryInto;

use anyhow::{Context, Result};

use crate::filesystem::Filesystem;
use crate::identifier::Identifier;

pub struct RootDevice {
    pub filesystem: Filesystem,
    pub identifier: Identifier,
    pub devpath: Option<String>,
    pub read_only: bool,
    /// Raw mount options from rootflags=, e.g. ["noatime", "discard"]
    pub mount_options: Vec<String>,
}

pub fn get_root_from_cmdline(cmdline: &[String]) -> Result<RootDevice> {
    let identifier = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root="))
        .last()
        .with_context(|| "no root= parameter found in kernel command line")?
        .strip_prefix("root=")
        .unwrap();

    let fs_str = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root.type="))
        .last()
        .and_then(|s| s.strip_prefix("root.type="))
        .unwrap_or("auto");

    // Root is read-only unless rw is the last of ro/rw to appear on the cmdline.
    let read_only = cmdline
        .iter()
        .filter(|arg| arg.as_str() == "ro" || arg.as_str() == "rw")
        .last()
        .map(|arg| arg == "ro")
        .unwrap_or(true);

    let mount_options = cmdline
        .iter()
        .filter_map(|arg| arg.strip_prefix("rootflags="))
        .last()
        .map(|opts| opts.split(',').map(str::to_string).collect())
        .unwrap_or_default();

    Ok(RootDevice {
        identifier: if let Some(uuid) = identifier.strip_prefix("UUID=") {
            Identifier::Uuid(uuid.to_string())
        } else {
            Identifier::Path(identifier.to_string())
        },
        filesystem: fs_str.try_into()?,
        devpath: None,
        read_only,
        mount_options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn test_root_by_path() {
        let cmdline = args("quiet root=/dev/sda1");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(matches!(&root.identifier, Identifier::Path(p) if p == "/dev/sda1"));
        assert_eq!(root.filesystem, Filesystem::Auto);
    }

    #[test]
    fn test_root_by_uuid() {
        let cmdline = args("root=UUID=aaaa-bbbb root.type=ext4");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(matches!(&root.identifier, Identifier::Uuid(u) if u == "aaaa-bbbb"));
        assert_eq!(root.filesystem, Filesystem::Ext4);
    }

    #[test]
    fn test_missing_root_param() {
        let cmdline = args("quiet splash");
        assert!(get_root_from_cmdline(&cmdline).is_err());
    }

    #[test]
    fn test_last_root_wins() {
        let cmdline = args("root=/dev/sda1 root=/dev/nvme0n1p2");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(matches!(&root.identifier, Identifier::Path(p) if p == "/dev/nvme0n1p2"));
    }

    #[test]
    fn test_default_read_only() {
        let cmdline = args("root=/dev/sda1");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(root.read_only);
    }

    #[test]
    fn test_rw_overrides_default() {
        let cmdline = args("root=/dev/sda1 rw");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(!root.read_only);
    }

    #[test]
    fn test_last_of_ro_rw_wins() {
        let cmdline = args("root=/dev/sda1 rw ro");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(root.read_only);

        let cmdline = args("root=/dev/sda1 ro rw");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(!root.read_only);
    }

    #[test]
    fn test_rootflags_parsing() {
        let cmdline = args("root=/dev/sda1 rootflags=noatime,discard,data=ordered");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(
            root.mount_options,
            vec!["noatime", "discard", "data=ordered"]
        );
    }

    #[test]
    fn test_no_rootflags_defaults_empty() {
        let cmdline = args("root=/dev/sda1");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(root.mount_options.is_empty());
    }
}
