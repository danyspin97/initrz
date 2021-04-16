use crate::utils::get_blkid_cache;

extern crate rpassword;

use anyhow::{bail, Context, Result};
use either::Right;
use libcryptsetup_rs::{CryptActivateFlags, CryptInit, CryptKeyfileFlags, EncryptionFormat};
use log::{trace, warn};
use mount_api::{Fs, FsmountFlags, FsopenFlags, Mount, MountAttrFlags, MoveMountFlags};

use std::convert::{TryFrom, TryInto};
use std::env;
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

const UUID_TAG: &str = "UUID";

#[derive(PartialEq, Eq)]
enum Filesystem {
    Auto,
    Ext4,
}

enum EncryptionType {
    Luks,
}

enum UnlockType {
    AskPassphrase,
    Key(String),
}

#[derive(PartialEq, Eq)]
enum Identifier {
    Path(String),
    Uuid(String),
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self {
            Identifier::Path(path) => write!(f, "{:?}", path),
            Identifier::Uuid(uuid) => write!(f, "{}", uuid),
        }
    }
}

pub struct RootDevice {
    filesystem: Filesystem,
    identifier: Identifier,
}

pub struct EncryptedDevice {
    name: String,
    identifier: Identifier,
    encryption_type: EncryptionType,
    unlock: UnlockType,
}

impl EncryptedDevice {
    pub fn from_line(line: String) -> Result<EncryptedDevice> {
        let mut split = line.split_whitespace();
        Ok(EncryptedDevice {
            name: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .to_string(),
            identifier: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .into(),
            encryption_type: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .try_into()?,
            unlock: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .into(),
        })
    }
}

pub struct DeviceHandler {
    root: RootDevice,
    root_mount: Option<Mount>,
    encrypted_devices: Vec<EncryptedDevice>,
}

impl DeviceHandler {
    pub fn init(crypttab_path: &str, cmdline: &Vec<String>) -> Result<DeviceHandler> {
        let encrypted_devices = match Path::new(crypttab_path).exists() {
            true => parse_crypttab(crypttab_path)?,
            false => Vec::new(),
        };

        // TODO: Mix the devices from the cmdline

        Ok(DeviceHandler {
            root: get_root_from_cmdline(cmdline)?,
            root_mount: None,
            encrypted_devices,
        })
    }

    pub fn listen(&mut self, device_rx: Receiver<String>, res_tx: Sender<Result<()>>) {
        let timeout = Duration::new(3, 0);
        loop {
            let received = device_rx.recv_timeout(timeout);
            if received.is_err() {
                break;
            }
            let received = received.unwrap();
            if res_tx.send(self.handle(&received)).is_err() {
                warn!("unable to send device mount result to main");
                break;
            }
        }
    }

    fn get_encrypted_device(&self, path: &str) -> Option<&EncryptedDevice> {
        let blkid_cache = get_blkid_cache();
        let uuid = blkid_cache.get_tag_value("UUID", path).unwrap_or(&"");
        self.encrypted_devices
            .iter()
            .find(|d| match &d.identifier {
                Identifier::Path(saved_path) => saved_path == path,
                Identifier::Uuid(_) => false,
            })
            .or_else(|| {
                self.encrypted_devices.iter().find(|d| match &d.identifier {
                    Identifier::Path(_) => false,
                    Identifier::Uuid(saved_uuid) => saved_uuid == uuid,
                })
            })
    }

    pub fn is_root(&self, devname: &str) -> bool {
        get_path_from_identifier(&self.root.identifier)
            .map(|path| path == devname)
            .unwrap_or(false)
    }

    pub fn mount_root(&mut self, devname: &str) -> Result<()> {
        let filesystem = CString::new(self.root.filesystem.get_filesystem_string(&devname)?)?;

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
            File::open("/")?.as_raw_fd(),
            "new_root",
            MoveMountFlags::empty(),
        )?;

        self.root_mount = Some(mount);

        Ok(())
    }

    pub fn move_root_mount(&self) -> Result<()> {
        let root = File::open("/")?;
        match &self.root_mount {
            Some(mount) => Ok(mount
                .move_mount(root.as_raw_fd(), "/", MoveMountFlags::empty())
                .with_context(|| "unable to move root device into /")?),
            None => bail!("unable to find root mount"),
        }
    }

    pub fn search_root(&mut self) -> Result<bool> {
        for device in get_blkid_cache().iter() {
            let devname = device.devname()?;
            let root_identifier = &self.root.identifier;
            if match root_identifier {
                Identifier::Uuid(uuid) => device
                    .tag_iter()
                    .find(|tag| tag == &(String::from(UUID_TAG), String::from(uuid)))
                    .is_some(),
                Identifier::Path(path) => devname == path,
            } {
                self.mount_root(devname)?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn unlock_available_devices(&self) -> Result<()> {
        for device in get_blkid_cache().iter() {
            let devname = device.devname()?;
            for encrypted_device in &self.encrypted_devices {
                if match &encrypted_device.identifier {
                    Identifier::Uuid(uuid) => device
                        .tag_iter()
                        .find(|tag| tag == &(String::from(UUID_TAG), String::from(uuid)))
                        .is_some(),
                    Identifier::Path(path) => devname == path,
                } {
                    unlock_luks_device(devname, &encrypted_device)?;
                }
            }
        }

        Ok(())
    }

    pub fn unlock_device(&self, path: &str, encrypted_device: &EncryptedDevice) -> Result<()> {
        match &encrypted_device.encryption_type {
            EncryptionType::Luks => unlock_luks_device(path, encrypted_device)?,
        };

        Ok(())
    }

    pub fn handle(&mut self, path: &str) -> Result<()> {
        if let Some(encrypted_device) = self.get_encrypted_device(&path) {
            // TODO: execute in another thread and save the result
            self.unlock_device(&path, encrypted_device)?;
            return Ok(());
        }

        if self.is_root(&path) {
            self.mount_root(&path)?;
            return Ok(());
        }

        let mut blkid_cache = get_blkid_cache();
        let filesystem = blkid_cache.get_tag_value("TYPE", &path);
        if filesystem.is_err() {
            // We have got a block device with no filesystem, just skip
            return Ok(());
        }
        let filesystem = filesystem.unwrap();
        if filesystem == "lvm" {
            let output = Command::new("/vgchange")
                .arg("-ay")
                .output()
                .with_context(|| "unable to run vgchange command")?;
            if !output.status.success() {
                bail!(
                    "vgchange command failed:\n{:?}",
                    String::from_utf8(output.stderr)
                )
            }

            let output = Command::new("/vgmknodes")
                .output()
                .with_context(|| "unable to run vgmknodes command")?;
            if !output.status.success() {
                bail!(
                    "vgmknodes command failed:\n{:?}",
                    String::from_utf8(output.stderr)
                )
            }
        }

        blkid_cache.probe_all_new()?;
        blkid_cache.put_cache();
        Ok(())
    }
}

fn get_path_from_identifier(identifier: &Identifier) -> Result<String> {
    Ok(match identifier {
        Identifier::Uuid(uuid) => {
            String::from(get_blkid_cache().get_devname(Right((&UUID_TAG, &uuid)))?)
        }
        Identifier::Path(path) => {
            if Path::new(path).exists() {
                bail!("unable to find device in path {:?}", path);
            }
            path.clone()
        }
    })
}

fn unlock_luks_device(path: &str, encrypted_device: &EncryptedDevice) -> Result<()> {
    let mut device = CryptInit::init(Path::new(path))?;
    device
        .context_handle()
        .load::<()>(Some(EncryptionFormat::Luks2), None)?;

    match &encrypted_device.unlock {
        UnlockType::Key(key) => {
            device.keyfile_handle().device_read(
                Path::new(key),
                0,
                None,
                CryptKeyfileFlags::empty(),
            )?;
        }
        UnlockType::AskPassphrase => {
            device.activate_handle().activate_by_passphrase(
                Some(&encrypted_device.name),
                None,
                ask_passphrase_for_device(encrypted_device)?.as_bytes(),
                CryptActivateFlags::empty(),
            )?;
        }
    };

    Ok(())
}

fn get_root_from_cmdline(cmdline: &Vec<String>) -> Result<RootDevice> {
    let auto_type = String::from("root.type=auto");

    let identifier = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root="))
        .last()
        .with_context(|| "unable to find root device from command lines")?
        .strip_prefix("root=")
        .unwrap();

    Ok(RootDevice {
        identifier: if let Some(uuid) = identifier.strip_prefix("UUID=") {
            Identifier::Uuid(String::from(uuid))
        } else {
            Identifier::Path(String::from(identifier))
        },
        filesystem: cmdline
            .iter()
            .filter(|arg| arg.starts_with("root.type="))
            .last()
            .or_else(|| Some(&auto_type))
            .map(|root| root.strip_prefix("root.type="))
            .unwrap()
            .unwrap()
            .try_into()?,
    })
}

fn parse_crypttab(crypttab_path: &str) -> Result<Vec<EncryptedDevice>> {
    let file =
        File::open(crypttab_path).with_context(|| format!("unable to open {:?}", crypttab_path))?;
    // TODO: Print errors
    Ok(BufReader::new(file)
        .lines()
        .filter_map(|line| line.ok())
        .filter(|line| !line.starts_with("#"))
        .filter(|line| !line.is_empty())
        .map(EncryptedDevice::from_line)
        .filter_map(|device| device.ok())
        .collect())
}

impl From<&str> for Identifier {
    fn from(identifier: &str) -> Identifier {
        if identifier.starts_with("UUID=") {
            Identifier::Uuid(identifier[5..].to_string())
        } else {
            Identifier::Path(identifier.to_string())
        }
    }
}

impl TryFrom<&str> for EncryptionType {
    type Error = anyhow::Error;

    fn try_from(encryption: &str) -> Result<EncryptionType> {
        Ok(match encryption {
            "luks" => EncryptionType::Luks,
            _ => bail!("{} is not a supported encryption type", encryption),
        })
    }
}

fn ask_passphrase_for_device(encrypted_device: &EncryptedDevice) -> Result<String> {
    Ok(rpassword::read_password_from_tty(Some(&format!(
        "Password for device {}: ",
        encrypted_device.identifier
    )))?)
}

impl From<&str> for UnlockType {
    fn from(unlock_type: &str) -> UnlockType {
        match unlock_type {
            "none" => UnlockType::AskPassphrase,
            _ => UnlockType::Key(unlock_type.into()),
        }
    }
}

impl TryFrom<&str> for Filesystem {
    type Error = anyhow::Error;

    fn try_from(filesystem: &str) -> Result<Self, Self::Error> {
        Ok(match filesystem {
            "ext4" => Filesystem::Ext4,
            "auto" => Filesystem::Auto,
            _ => bail!("{} is not a supported filesystem", filesystem),
        })
    }
}

impl Filesystem {
    fn get_filesystem_string(&self, path: &str) -> Result<String> {
        Ok(match self {
            Filesystem::Ext4 => String::from("ext4"),
            Filesystem::Auto => String::from(
                get_blkid_cache()
                    .get_tag_value("TYPE", path)
                    .with_context(|| {
                        format!("unable to get filesystem type for device {:?}", path)
                    })?,
            ),
        })
    }
}
