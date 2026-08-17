use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::{ioctl_none_bad, ioctl_write_int_bad};

// From <linux/loop.h>. Both request numbers are already plain `_IO()`-encoded (type 0x4C, dir
// "none", no size bits) - the "_bad" nix macros are used (rather than ioctl_none!/
// ioctl_write_int!) only because they take the pre-encoded request number as-is instead of
// re-deriving the direction/size bits from a data type, which matters for LOOP_SET_FD: it takes
// a bare fd as its third ioctl argument rather than a pointer to a buffer, so the standard
// write-ioctl macros (which assume a *pointer* argument) don't apply here.
const LOOP_SET_FD: libc::c_ulong = 0x4C00;
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;

ioctl_none_bad!(loop_ctl_get_free, LOOP_CTL_GET_FREE);
ioctl_write_int_bad!(loop_set_fd, LOOP_SET_FD);

/// Attach `image_path` to a free loop device and return its path (e.g. "/dev/loop0").
///
/// This is the native equivalent of `losetup`: it lets a plain filesystem image file (e.g. a
/// squashfs root image, as dracut's dmsquash-live module uses) be mounted as if it were a block
/// device.
pub fn attach(image_path: &Path) -> Result<PathBuf> {
    let ctl =
        File::open("/dev/loop-control").with_context(|| "unable to open /dev/loop-control")?;

    let index = unsafe { loop_ctl_get_free(ctl.as_raw_fd()) }
        .with_context(|| "LOOP_CTL_GET_FREE failed")?;

    let loop_path = PathBuf::from(format!("/dev/loop{}", index));
    let loop_dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&loop_path)
        .with_context(|| format!("unable to open {:?}", loop_path))?;

    let backing_file = File::open(image_path)
        .with_context(|| format!("unable to open backing image {:?}", image_path))?;

    unsafe { loop_set_fd(loop_dev.as_raw_fd(), backing_file.as_raw_fd()) }.with_context(|| {
        format!(
            "LOOP_SET_FD failed attaching {:?} to {:?}",
            image_path, loop_path
        )
    })?;

    Ok(loop_path)
}
