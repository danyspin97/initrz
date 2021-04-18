extern crate rpassword;

use anyhow::{bail, Context, Result};
use libcryptsetup_rs::{CryptActivateFlags, CryptInit, CryptKeyfileFlags, EncryptionFormat};

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Receiver;

use crate::encrypted_device::EncryptedDevice;
use crate::encryption_type::EncryptionType;
use crate::identifier::Identifier;
use crate::root_device::{get_root_from_cmdline, RootDevice};
use crate::unlock_type::UnlockType;
use crate::utils::get_blkid_cache;

const UUID_TAG: &str = "UUID";

pub struct DeviceHandler {
    root: RootDevice,
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
            encrypted_devices,
        })
    }

    pub fn listen(&mut self, device_rx: Receiver<String>) -> Result<()> {
        loop {
            let received = device_rx.try_recv();
            if received.is_err() {
                match received.unwrap_err() {
                    std::sync::mpsc::TryRecvError::Disconnected => break,
                    _ => {}
                }
                break;
            }
            let received = received.unwrap();
            self.handle(&received)?;
        }

        Ok(())
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

    pub fn get_root(self) -> Option<RootDevice> {
        if self.root.devpath.is_some() {
            Some(self.root)
        } else {
            None
        }
    }

    pub fn is_root(&self, devname: &str) -> bool {
        self.root
            .identifier
            .get_path()
            .map(|path| path == devname)
            .unwrap_or(false)
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
                self.root.devpath = Some(devname.to_string());
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
            self.root.devpath = Some(path.to_string());
            return Ok(());
        }

        let mut blkid_cache = get_blkid_cache();
        blkid_cache.probe_all_new()?;

        let filesystem = blkid_cache.get_tag_value("TYPE", &path);
        if filesystem.is_err() {
            // We have got a block device with no filesystem, skip
            return Ok(());
        }
        let filesystem = filesystem.unwrap();
        if filesystem == "lvm" {
            let output = Command::new("/bin/vgchange")
                .arg("-ay")
                .output()
                .with_context(|| "unable to run vgchange command")?;
            if !output.status.success() {
                bail!(
                    "vgchange command failed:\n{:?}",
                    String::from_utf8(output.stderr)
                )
            }

            let output = Command::new("/bin/vgmknodes")
                .output()
                .with_context(|| "unable to run vgmknodes command")?;
            if !output.status.success() {
                bail!(
                    "vgmknodes command failed:\n{:?}",
                    String::from_utf8(output.stderr)
                )
            }
        }

        blkid_cache.put_cache();
        Ok(())
    }
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

fn ask_passphrase_for_device(encrypted_device: &EncryptedDevice) -> Result<String> {
    Ok(rpassword::read_password_from_tty(Some(&format!(
        "Password for device {}: ",
        encrypted_device.identifier
    )))?)
}
