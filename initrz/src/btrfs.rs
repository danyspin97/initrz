use std::fs::File;
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use nix::ioctl_write_ptr;

// From <linux/btrfs.h>: BTRFS_IOCTL_MAGIC = 0x94, BTRFS_IOC_SCAN_DEV = _IOW(0x94, 4, struct
// btrfs_ioctl_vol_args). The ioctl is issued on /dev/btrfs-control (not the device itself) and
// registers the device with the kernel's multi-device btrfs layer, which is otherwise only done
// by udev rules / `btrfs device scan` in a regular userspace.
const BTRFS_PATH_NAME_MAX: usize = 4087;

#[repr(C)]
struct BtrfsIoctlVolArgs {
    fd: i64,
    name: [u8; BTRFS_PATH_NAME_MAX + 1],
}

ioctl_write_ptr!(btrfs_ioc_scan_dev, 0x94, 4, BtrfsIoctlVolArgs);

/// Register `devpath` with the kernel's btrfs multi-device layer via BTRFS_IOC_SCAN_DEV.
///
/// This must happen for every member device of a multi-device btrfs filesystem before it can be
/// mounted; single-device btrfs filesystems don't need it. Requires the btrfs module to already
/// be loaded (it creates /dev/btrfs-control).
pub fn scan_device(devpath: &str) -> Result<()> {
    let bytes = devpath.as_bytes();
    if bytes.len() > BTRFS_PATH_NAME_MAX {
        anyhow::bail!("device path {:?} too long for btrfs ioctl", devpath);
    }

    let ctrl =
        File::open("/dev/btrfs-control").with_context(|| "unable to open /dev/btrfs-control")?;

    let mut args = BtrfsIoctlVolArgs {
        fd: 0,
        name: [0u8; BTRFS_PATH_NAME_MAX + 1],
    };
    args.name[..bytes.len()].copy_from_slice(bytes);

    unsafe { btrfs_ioc_scan_dev(ctrl.as_raw_fd(), &args) }
        .with_context(|| format!("BTRFS_IOC_SCAN_DEV failed for {:?}", devpath))?;

    Ok(())
}
