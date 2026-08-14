use std::convert::TryInto;

use anyhow::{bail, Context, Result};

use crate::filesystem::Filesystem;
use crate::identifier::Identifier;

pub struct RootDevice {
    pub filesystem: Filesystem,
    pub identifier: Identifier,
    pub devpath: Option<String>,
    pub read_only: bool,
    /// Raw mount options from rootflags=, e.g. ["noatime", "discard"]
    pub mount_options: Vec<String>,
    /// Set when `rd.live.overlay[=<dir>]` is on the cmdline: the resolved root is mounted
    /// read-only and used as the lowerdir of an overlayfs, rather than being the final root
    /// itself. This is the core mechanism behind dracut's dmsquash-live writable overlay.
    pub overlay: Option<OverlayConfig>,
}

pub struct OverlayConfig {
    /// Directory to hold the overlay's upper/work subdirectories. `None` means: mount a fresh
    /// tmpfs and use that (an ephemeral, boot-to-boot overlay - the common live-boot case).
    /// `Some(dir)` means: use `<dir>/upper` and `<dir>/work` directly, so changes can persist
    /// across boots if `dir` is itself on persistent storage. `dir` must already be reachable
    /// (e.g. on a tmpfs or a filesystem mounted earlier) - resolving it from a separate block
    /// device (as dracut's rd.live.overlay can) is out of scope.
    pub persistent_dir: Option<String>,
}

pub fn get_root_from_cmdline(cmdline: &[String]) -> Result<RootDevice> {
    let identifier = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root="))
        .last()
        .with_context(|| "no root= parameter found in kernel command line")?
        .strip_prefix("root=")
        .unwrap();

    // Root is read-only unless rw is the last of ro/rw to appear on the cmdline.
    let read_only = cmdline
        .iter()
        .filter(|arg| arg.as_str() == "ro" || arg.as_str() == "rw")
        .last()
        .map(|arg| arg == "ro")
        .unwrap_or(true);

    let mut mount_options: Vec<String> = cmdline
        .iter()
        .filter_map(|arg| arg.strip_prefix("rootflags="))
        .last()
        .map(|opts| opts.split(',').map(str::to_string).collect())
        .unwrap_or_default();

    let overlay = parse_overlay(cmdline);

    let nfsroot = cmdline
        .iter()
        .filter_map(|arg| arg.strip_prefix("nfsroot="))
        .last();

    // NFS root: root=nfs:<server>:<path>, root=nfs4:<server>:<path>, or the legacy
    // root=/dev/nfs paired with nfsroot=<server>:<path>[,<options>]. NFSv4 only - see the
    // Filesystem::Nfs4 doc comment for why NFSv3 isn't supported.
    if let Some(server_path) = nfs_server_path(identifier, nfsroot)? {
        if let Some(nfsroot) = nfsroot {
            let (_, options) = split_nfsroot(nfsroot);
            mount_options.extend(options.split(',').filter(|s| !s.is_empty()).map(str::to_string));
        }
        if !mount_options.iter().any(|opt| opt.starts_with("vers=")) {
            mount_options.push("vers=4.2".to_string());
        }
        return Ok(RootDevice {
            identifier: Identifier::Path(server_path.clone()),
            filesystem: Filesystem::Nfs4,
            devpath: Some(server_path),
            read_only,
            mount_options,
            overlay,
        });
    }

    // CIFS root: root=cifs://<server>/<share>. Credentials/extra options come through the
    // regular rootflags= mount_options above, either directly (e.g.
    // rootflags=username=foo,password=bar) or via rootflags=credentials=<file> to avoid putting
    // a password in plain text on the kernel cmdline - see cifs_credentials.rs.
    if let Some(share) = identifier.strip_prefix("cifs://") {
        let devpath = format!("//{}", share);
        return Ok(RootDevice {
            identifier: Identifier::Path(devpath.clone()),
            filesystem: Filesystem::Cifs,
            devpath: Some(devpath),
            read_only,
            mount_options,
            overlay,
        });
    }

    let fs_str = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root.type="))
        .last()
        .and_then(|s| s.strip_prefix("root.type="))
        .unwrap_or("auto");

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
        overlay,
    })
}

/// Parses `rd.live.overlay` (ephemeral tmpfs overlay) or `rd.live.overlay=<dir>` (persistent
/// overlay under `<dir>`) from the cmdline.
fn parse_overlay(cmdline: &[String]) -> Option<OverlayConfig> {
    cmdline.iter().rev().find_map(|arg| {
        if arg == "rd.live.overlay" {
            Some(OverlayConfig { persistent_dir: None })
        } else {
            arg.strip_prefix("rd.live.overlay=").map(|dir| OverlayConfig {
                persistent_dir: Some(dir.to_string()),
            })
        }
    })
}

/// Returns the `server:path` portion of an NFS root, if `identifier` names one - either
/// `nfs:server:path` / `nfs4:server:path` directly, or `/dev/nfs` with the server:path coming
/// from `nfsroot=`.
fn nfs_server_path(identifier: &str, nfsroot: Option<&str>) -> Result<Option<String>> {
    let server_path = if let Some(rest) = identifier.strip_prefix("nfs4:") {
        rest.to_string()
    } else if let Some(rest) = identifier.strip_prefix("nfs:") {
        rest.to_string()
    } else if identifier == "/dev/nfs" {
        let nfsroot =
            nfsroot.with_context(|| "root=/dev/nfs requires nfsroot=<server>:<path>")?;
        split_nfsroot(nfsroot).0.to_string()
    } else {
        return Ok(None);
    };

    if !server_path.contains(':') {
        bail!("invalid NFS root {:?}: expected server:path", server_path);
    }

    Ok(Some(server_path))
}

/// Splits `nfsroot=<server>:<path>[,<options>]` into its `server:path` and `options` parts.
fn split_nfsroot(nfsroot: &str) -> (&str, &str) {
    match nfsroot.split_once(',') {
        Some((server_path, options)) => (server_path, options),
        None => (nfsroot, ""),
    }
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

    #[test]
    fn test_nfs4_root_direct() {
        let cmdline = args("root=nfs4:192.168.1.1:/export/root");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(root.filesystem, Filesystem::Nfs4);
        assert_eq!(root.devpath.as_deref(), Some("192.168.1.1:/export/root"));
        assert!(root.mount_options.contains(&"vers=4.2".to_string()));
    }

    #[test]
    fn test_nfs_root_legacy_dev_nfs() {
        let cmdline = args("root=/dev/nfs nfsroot=192.168.1.1:/export/root,noatime");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(root.filesystem, Filesystem::Nfs4);
        assert_eq!(root.devpath.as_deref(), Some("192.168.1.1:/export/root"));
        assert!(root.mount_options.contains(&"noatime".to_string()));
        assert!(root.mount_options.contains(&"vers=4.2".to_string()));
    }

    #[test]
    fn test_nfs_root_dev_nfs_without_nfsroot_errors() {
        let cmdline = args("root=/dev/nfs");
        assert!(get_root_from_cmdline(&cmdline).is_err());
    }

    #[test]
    fn test_nfs_root_does_not_clobber_explicit_vers() {
        let cmdline = args("root=nfs:192.168.1.1:/export/root rootflags=vers=4.0");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(
            root.mount_options.iter().filter(|o| o.starts_with("vers=")).count(),
            1
        );
        assert!(root.mount_options.contains(&"vers=4.0".to_string()));
    }

    #[test]
    fn test_cifs_root() {
        let cmdline = args("root=cifs://192.168.1.1/share rootflags=username=foo,password=bar");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(root.filesystem, Filesystem::Cifs);
        assert_eq!(root.devpath.as_deref(), Some("//192.168.1.1/share"));
        assert!(root.mount_options.contains(&"username=foo".to_string()));
        assert!(root.mount_options.contains(&"password=bar".to_string()));
    }

    #[test]
    fn test_no_overlay_by_default() {
        let cmdline = args("root=/dev/sda1");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(root.overlay.is_none());
    }

    #[test]
    fn test_ephemeral_overlay() {
        let cmdline = args("root=/dev/sda1 rd.live.overlay");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert!(root.overlay.unwrap().persistent_dir.is_none());
    }

    #[test]
    fn test_persistent_overlay() {
        let cmdline = args("root=/dev/sda1 rd.live.overlay=/run/persist");
        let root = get_root_from_cmdline(&cmdline).unwrap();
        assert_eq!(
            root.overlay.unwrap().persistent_dir.as_deref(),
            Some("/run/persist")
        );
    }
}
