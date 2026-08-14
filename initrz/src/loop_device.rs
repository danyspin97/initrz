use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// From <linux/loop.h>. These are legacy raw ioctl numbers (not run through the standard
// _IOC()/_IOW() encoding macros), so we call libc::ioctl() directly instead of going through
// nix's ioctl_*! macros, which assume the standard encoding.
const LOOP_SET_FD: libc::c_ulong = 0x4C00;
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;

/// Attach `image_path` to a free loop device and return its path (e.g. "/dev/loop0").
///
/// This is the native equivalent of `losetup`: it lets a plain filesystem image file (e.g. a
/// squashfs root image, as dracut's dmsquash-live module uses) be mounted as if it were a block
/// device.
pub fn attach(image_path: &Path) -> Result<PathBuf> {
    let ctl = File::open("/dev/loop-control")
        .with_context(|| "unable to open /dev/loop-control")?;

    let index = unsafe { libc::ioctl(ctl.as_raw_fd(), LOOP_CTL_GET_FREE) };
    if index < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| "LOOP_CTL_GET_FREE failed");
    }

    let loop_path = PathBuf::from(format!("/dev/loop{}", index));
    let loop_dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&loop_path)
        .with_context(|| format!("unable to open {:?}", loop_path))?;

    let backing_file = File::open(image_path)
        .with_context(|| format!("unable to open backing image {:?}", image_path))?;

    let ret = unsafe {
        libc::ioctl(
            loop_dev.as_raw_fd(),
            LOOP_SET_FD,
            backing_file.as_raw_fd() as libc::c_long,
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "LOOP_SET_FD failed attaching {:?} to {:?}",
                image_path, loop_path
            )
        });
    }

    Ok(loop_path)
}
