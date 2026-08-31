use std::{
    env,
    ffi::CString,
    fs::{self, File},
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

/// Where the root filesystem is mounted read-only when it's being used as an overlay's lowerdir
/// rather than as the final root directly.
const OVERLAY_LOWER_DIR: &str = "/run/rootfs.ro";

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

        let mount = match &root.overlay {
            None => open_and_mount(
                module_loader,
                &filesystem,
                &devpath,
                &root.mount_options,
                root.read_only,
            )?,
            Some(overlay) => {
                // Mount the resolved root read-only as the overlay's lowerdir, then mount an
                // overlayfs combining it with a writable upperdir/workdir as the actual root.
                // This is the core mechanism behind dracut's dmsquash-live writable overlay.
                fs::create_dir_all(OVERLAY_LOWER_DIR)
                    .with_context(|| format!("unable to create {:?}", OVERLAY_LOWER_DIR))?;
                let lower_dir = File::open(OVERLAY_LOWER_DIR)
                    .with_context(|| format!("unable to open {:?}", OVERLAY_LOWER_DIR))?;
                // rootflags= (root.mount_options) must still reach the lowerdir mount here - it's
                // the actual mount of the resolved root device, just read-only and relocated
                // under the overlay instead of becoming the final root directly.
                let lower_mount = open_and_mount(
                    module_loader,
                    &filesystem,
                    &devpath,
                    &root.mount_options,
                    true,
                )?;
                lower_mount
                    .move_mount(lower_dir.as_raw_fd(), ".", MoveMountFlags::empty())
                    .with_context(|| format!("unable to mount root at {:?}", OVERLAY_LOWER_DIR))?;

                let (upper_dir, work_dir) = match &overlay.persistent_dir {
                    Some(dir) => (format!("{}/upper", dir), format!("{}/work", dir)),
                    None => {
                        fs::create_dir_all("/run/overlay")
                            .with_context(|| "unable to create /run/overlay")?;
                        // Dropping this Mount handle only closes its control fd (see
                        // mount-api's Drop impl) - it does not unmount; the tmpfs stays attached
                        // at /run/overlay once move_mount() has placed it there.
                        mount_special_filesystem(
                            self.root_file.as_raw_fd(),
                            "run/overlay",
                            "tmpfs",
                        )
                        .with_context(|| "unable to mount tmpfs for overlay upper/work dirs")?;
                        (
                            "/run/overlay/upper".to_string(),
                            "/run/overlay/work".to_string(),
                        )
                    }
                };
                fs::create_dir_all(&upper_dir)
                    .with_context(|| format!("unable to create {:?}", upper_dir))?;
                fs::create_dir_all(&work_dir)
                    .with_context(|| format!("unable to create {:?}", work_dir))?;

                let overlay_options = vec![
                    format!("lowerdir={}", OVERLAY_LOWER_DIR),
                    format!("upperdir={}", upper_dir),
                    format!("workdir={}", work_dir),
                ];
                open_and_mount(
                    module_loader,
                    "overlay",
                    "overlay",
                    &overlay_options,
                    root.read_only,
                )?
            }
        };

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

/// Opens a filesystem context of type `fstype`, points it at `source`, applies `options`
/// (falling back to `read_only` for the ones not explicitly overridden), and mounts it
/// (unattached - the caller still needs to `move_mount` it somewhere). Shared by the plain
/// root-mount path and the two-stage overlay path in `mount_root()`.
fn open_and_mount(
    module_loader: &ModuleLoader,
    fstype: &str,
    source: &str,
    options: &[String],
    read_only: bool,
) -> Result<Mount> {
    if !module_loader.load_module(fstype)? {
        // Do not fail here because the module could be builtin
        warn!("module {} not found", fstype);
    }
    if fstype == "btrfs" {
        register_btrfs_devices();
    }
    // The kernel's cifs.ko has no notion of credentials=<file> (only the userspace mount.cifs
    // tool translates that) - expand it into username=/password=/domain= ourselves so a CIFS
    // root's password doesn't have to be passed in plain text via rootflags= on the cmdline.
    let owned_options;
    let options = if fstype == "cifs" {
        owned_options = crate::cifs_credentials::resolve(options)?;
        owned_options.as_slice()
    } else {
        options
    };
    let fstype = CString::new(fstype)?;

    let fs = Fs::open(&fstype, FsopenFlags::empty())
        .with_context(|| format!("unable to open a filesystem context of type {:?}", fstype))?;
    let source_str: CString = CString::new("source")?;
    let source_c = CString::new(source)?;
    fs.set_string(&source_str, &source_c)
        .with_context(|| format!("unable to set source {:?} for filesystem", source_c))?;

    let mut attr_flags = MountAttrFlags::empty();
    if read_only {
        attr_flags |= MountAttrFlags::MOUNT_ATTR_RDONLY;
    }
    for option in options {
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
            "unable to create filesystem context of type {:?} for source {:?}",
            fstype, source_c
        )
    })?;
    fs.mount(FsmountFlags::empty(), attr_flags)
        .with_context(|| format!("unable to mount {:?}", source_c))
}

/// Register every currently-known btrfs device with the kernel's multi-device layer, so a
/// multi-device btrfs root can be mounted from any one member device. Failures are logged and
/// skipped rather than propagated: a single-device btrfs root doesn't need this at all, and a
/// device that's already registered (or unreadable) shouldn't block the mount attempt.
fn register_btrfs_devices() {
    let Ok(mut cache) = get_blkid_cache() else {
        return;
    };
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
        if device
            .tag_iter()
            .any(|tag| tag == ("TYPE".to_string(), "btrfs".to_string()))
        {
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
