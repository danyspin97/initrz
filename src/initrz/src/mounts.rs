use std::{
    env,
    ffi::CString,
    fs::File,
    os::unix::io::{AsRawFd, RawFd},
};

use anyhow::{Context, Result};
use log::warn;
use mount_api::{Fs, FsmountFlags, FsopenFlags, Mount, MountAttrFlags, MoveMountFlags};

use crate::module_loader::ModuleLoader;
use crate::root_device::RootDevice;

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

        let devname = root.devpath.unwrap();
        let filesystem = root.filesystem.get_filesystem_string(&devname)?;
        if !module_loader.load_module(&filesystem)? {
            // Do not fail here because the module could be builtin
            warn!("module {} not found", filesystem);
        }
        let filesystem = CString::new(filesystem)?;

        let fs = Fs::open(&filesystem, FsopenFlags::empty()).with_context(|| {
            format!(
                "unable to open a filesystem context of type {:?}",
                &filesystem
            )
        })?;
        let source_str: CString = CString::new("source")?;
        let devname = CString::new(devname)?;
        fs.set_string(&source_str, &devname)
            .with_context(|| format!("unable to set source {:?} for filesystem", devname))?;
        fs.create().with_context(|| {
            format!(
                "unable to create filesystem context for type {:?}",
                &filesystem
            )
        })?;
        let mount = fs
            .mount(FsmountFlags::empty(), MountAttrFlags::empty())
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
                Ok(mount
                    .move_mount(
                        new_root_dir.as_raw_fd(),
                        name.as_str(),
                        MoveMountFlags::empty(),
                    )
                    .with_context(|| "unable to move root device into /")?)
            })?;

        env::set_current_dir("/new_root")?;

        mount
            .move_mount(self.root_file.as_raw_fd(), ".", MoveMountFlags::empty())
            .with_context(|| "unable to move root device into /")?;

        Ok(())
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
