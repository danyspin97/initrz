use std::{
    env,
    ffi::CString,
    fs::File,
    os::unix::io::{AsRawFd, RawFd},
    path::Path,
};

use anyhow::{Context, Result};
use log::warn;
use mount_api::{Fs, FsmountFlags, FsopenFlags, Mount, MountAttrFlags, MoveMountFlags};

use crate::btrfs;
use crate::loop_device;
use crate::module_loader::ModuleLoader;
use crate::root_device::RootDevice;
use crate::utils::get_blkid_cache;

pub struct Mounts {
    mountpoints: Vec<(String, Mount)>,
    root_file: File,
}

impl Mounts {
    pub fn with_default_mounts() -> Result<Mounts> {
        let root_file = File::open("/").with_context(|| "unable to open / dir")?;
        Ok(Mounts {
            mountpoints: [("dev", "devtmpfs"), ("sys", "sysfs"), ("proc", "proc")]
                .iter()
                .map(|(name, fs)| -> Result<(String, Mount)> {
                    Ok((
                        name.to_string(),
                        mount_special_filesystem(root_file.as_raw_fd(), name, fs)
                            .with_context(|| format!("unable to mount /{}", name))?,
                    ))
                })
                .collect::<Result<Vec<(_, _)>>>()?,
            root_file,
        })
    }

    pub fn mount_root(&self, root: RootDevice, module_loader: &ModuleLoader) -> Result<()> {
        // Load essential module
        module_loader.load_module("crc32c_generic")?;

        let devpath = root.devpath.unwrap();
        // If root resolved to a plain image file (e.g. a squashfs image, as with dracut's
        // dmsquash-live) rather than a block device, loop-attach it first so the rest of the
        // mount path can treat it exactly like any other block device.
        let devpath = if Path::new(&devpath).is_file() {
            loop_device::attach(Path::new(&devpath))
                .with_context(|| format!("unable to loop-attach root image {:?}", devpath))?
                .to_str()
                .with_context(|| "loop device path is not valid UTF-8")?
                .to_string()
        } else {
            devpath
        };
        let filesystem = root.filesystem.get_filesystem_string(&devpath)?;
        if !module_loader.load_module(&filesystem)? {
            // Do not fail here because the module could be builtin
            warn!("module {} not found", filesystem);
        }
        if filesystem == "btrfs" {
            register_btrfs_devices();
        }
        let filesystem = CString::new(filesystem)?;

        let fs = Fs::open(&filesystem, FsopenFlags::empty()).with_context(|| {
            format!(
                "unable to open a filesystem context of type {:?}",
                &filesystem
            )
        })?;
        let source_str: CString = CString::new("source")?;
        let devname = CString::new(devpath.clone())?;
        fs.set_string(&source_str, &devname)
            .with_context(|| format!("unable to set source {:?} for filesystem", devname))?;

        let mut attr_flags = MountAttrFlags::empty();
        if root.read_only {
            attr_flags |= MountAttrFlags::MOUNT_ATTR_RDONLY;
        }
        for option in &root.mount_options {
            match option.as_str() {
                "ro" => attr_flags |= MountAttrFlags::MOUNT_ATTR_RDONLY,
                "rw" => attr_flags.remove(MountAttrFlags::MOUNT_ATTR_RDONLY),
                "noatime" => attr_flags |= MountAttrFlags::MOUNT_ATTR_NOATIME,
                "nodiratime" => attr_flags |= MountAttrFlags::MOUNT_ATTR_NODIRATIME,
                "relatime" => attr_flags |= MountAttrFlags::MOUNT_ATTR_RELATIME,
                "strictatime" => attr_flags |= MountAttrFlags::MOUNT_ATTR_STRICTATIME,
                "nosuid" => attr_flags |= MountAttrFlags::MOUNT_ATTR_NOSUID,
                "nodev" => attr_flags |= MountAttrFlags::MOUNT_ATTR_NODEV,
                "noexec" => attr_flags |= MountAttrFlags::MOUNT_ATTR_NOEXEC,
                // Anything else is a filesystem-specific option (e.g. ext4's
                // "discard" or "data=ordered"); pass it straight to the
                // filesystem context.
                _ => match option.split_once('=') {
                    Some((key, value)) => fs
                        .set_string(&CString::new(key)?, &CString::new(value)?)
                        .with_context(|| format!("unable to set mount option {}", option))?,
                    None => fs
                        .set_flag(&CString::new(option.as_str())?)
                        .with_context(|| format!("unable to set mount option {}", option))?,
                },
            }
        }

        fs.create().with_context(|| {
            format!(
                "unable to create filesystem context of type {:?} for device {:?}",
                &filesystem, devname
            )
        })?;
        let mount = fs
            .mount(FsmountFlags::empty(), attr_flags)
            .with_context(|| format!("unable to mount {:?}", devname))?;

        mount.move_mount(
            self.root_file.as_raw_fd(),
            "new_root",
            MoveMountFlags::empty(),
        )?;

        let new_root_dir = File::open("/new_root")?;
        self.mountpoints
            .iter()
            .try_for_each(|(name, mount)| -> Result<()> {
                mount
                    .move_mount(
                        new_root_dir.as_raw_fd(),
                        name.as_str(),
                        MoveMountFlags::empty(),
                    )
                    .with_context(|| "unable to move root device into /")
            })?;

        env::set_current_dir("/new_root")?;

        mount
            .move_mount(self.root_file.as_raw_fd(), ".", MoveMountFlags::empty())
            .with_context(|| "unable to move root device into /")?;

        Ok(())
    }
}

/// Register every currently-known btrfs device with the kernel's multi-device layer, so a
/// multi-device btrfs root can be mounted from any one member device. Failures are logged and
/// skipped rather than propagated: a single-device btrfs root doesn't need this at all, and a
/// device that's already registered (or unreadable) shouldn't block the mount attempt.
fn register_btrfs_devices() {
    let mut cache = get_blkid_cache();
    if cache.probe_all_new().is_err() {
        return;
    }
    for device in cache.iter() {
        let Ok(devname) = device.devname() else {
            continue;
        };
        let Some(devname) = devname.to_str() else {
            continue;
        };
        if device.tag_iter().any(|tag| tag == ("TYPE".to_string(), "btrfs".to_string())) {
            if let Err(err) = btrfs::scan_device(devname) {
                warn!("unable to register btrfs device {:?}: {:?}", devname, err);
            }
        }
    }
}

fn mount_special_filesystem(parent_dir: RawFd, mount_folder: &str, fs_name: &str) -> Result<Mount> {
    let fs = Fs::open(&CString::new(fs_name)?, FsopenFlags::empty())
        .with_context(|| format!("failed to open a filesystem context of type {}", fs_name))?;
    fs.create()
        .with_context(|| format!("unable to create filesystem context for type {}", fs_name))?;
    let mount = fs.mount(FsmountFlags::empty(), MountAttrFlags::empty())?;
    mount.move_mount(parent_dir, mount_folder, MoveMountFlags::empty())?;
    Ok(mount)
}
