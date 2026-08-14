use anyhow::{bail, Context, Result};
use libcryptsetup_rs::consts::flags::CryptActivate;
use libcryptsetup_rs::consts::vals::EncryptionFormat;
use libcryptsetup_rs::CryptInit;

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::encrypted_device::EncryptedDevice;
use crate::encryption_type::EncryptionType;
use crate::identifier::Identifier;
use crate::root_device::{get_root_from_cmdline, RootDevice};
use crate::unlock_type::UnlockType;
use crate::utils::get_blkid_cache;

const UUID_TAG: &str = "UUID";

pub struct DeviceHandler {
    root: RootDevice,
    pub(crate) encrypted_devices: Vec<EncryptedDevice>,
}

impl DeviceHandler {
    pub fn init(crypttab_path: &str, cmdline: &[String]) -> Result<DeviceHandler> {
        let mut encrypted_devices = if Path::new(crypttab_path).exists() {
            parse_crypttab(crypttab_path)?
        } else {
            Vec::new()
        };

        // Support dracut-compatible rd.luks.uuid=<UUID> on the kernel cmdline.
        // Each such parameter adds an encrypted device to unlock interactively.
        // rd.luks.name=<UUID>:<name> sets the dm-crypt target name; otherwise
        // the name defaults to "luks-<UUID>".
        let named: std::collections::HashMap<String, String> = cmdline
            .iter()
            .filter_map(|arg| arg.strip_prefix("rd.luks.name="))
            .filter_map(|val| {
                let mut parts = val.splitn(2, ':');
                let uuid = parts.next()?.to_string();
                let name = parts.next()?.to_string();
                Some((uuid, name))
            })
            .collect();

        for arg in cmdline {
            if let Some(uuid) = arg.strip_prefix("rd.luks.uuid=") {
                let name = named
                    .get(uuid)
                    .cloned()
                    .unwrap_or_else(|| format!("luks-{}", uuid));
                // Skip if already present from crypttab
                if !encrypted_devices
                    .iter()
                    .any(|d| matches!(&d.identifier, Identifier::Uuid(u) if u == uuid))
                {
                    encrypted_devices.push(EncryptedDevice {
                        name,
                        identifier: Identifier::Uuid(uuid.to_string()),
                        encryption_type: EncryptionType::Luks,
                        unlock: UnlockType::AskPassphrase,
                        unlocked: false,
                    });
                }
            }
        }

        Ok(DeviceHandler {
            root: get_root_from_cmdline(cmdline)?,
            encrypted_devices,
        })
    }

    fn is_ready(&self) -> bool {
        let all_unlocked = self.encrypted_devices.iter().all(|d| d.unlocked);
        self.root.devpath.is_some() && all_unlocked
    }

    pub fn listen(&mut self, device_rx: Receiver<String>, timeout: Duration) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!(
                    "timed out waiting for root device after {}s",
                    timeout.as_secs()
                );
            }
            match device_rx.recv_timeout(remaining) {
                Ok(path) => {
                    self.handle(&path)?;
                    if self.is_ready() {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    anyhow::bail!(
                        "timed out waiting for root device after {}s",
                        timeout.as_secs()
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("uevent listener thread exited unexpectedly");
                }
            }
        }
    }

    fn get_encrypted_device_mut(&mut self, path: &str) -> Option<&mut EncryptedDevice> {
        let blkid_cache = get_blkid_cache();
        let uuid = blkid_cache
            .get_tag_value("UUID", &PathBuf::from(path))
            .unwrap_or_default();
        // Try path match first, then UUID match
        if let Some(pos) = self.encrypted_devices.iter().position(|d| match &d.identifier {
            Identifier::Path(saved_path) => saved_path == path,
            Identifier::Uuid(saved_uuid) => saved_uuid == &uuid,
        }) {
            Some(&mut self.encrypted_devices[pos])
        } else {
            None
        }
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
                    .any(|tag| tag == (String::from(UUID_TAG), String::from(uuid))),
                Identifier::Path(path) => devname.to_str().unwrap() == path,
            } {
                self.root.devpath = Some(devname.to_str().unwrap().to_string());
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn unlock_available_devices(&mut self) -> Result<()> {
        for device in get_blkid_cache().iter() {
            let devname = device.devname()?;
            let devname_str = devname.to_str().unwrap();
            if let Some(pos) = self.encrypted_devices.iter().position(|enc| match &enc.identifier {
                Identifier::Uuid(uuid) => device
                    .tag_iter()
                    .any(|tag| tag == (String::from(UUID_TAG), String::from(uuid))),
                Identifier::Path(path) => devname_str == path,
            }) {
                unlock_luks_device(devname_str, &self.encrypted_devices[pos])?;
                self.encrypted_devices[pos].unlocked = true;
            }
        }

        Ok(())
    }

    pub fn handle(&mut self, path: &str) -> Result<()> {
        if let Some(encrypted_device) = self.get_encrypted_device_mut(path) {
            if !encrypted_device.unlocked {
                unlock_luks_device(path, encrypted_device)?;
                encrypted_device.unlocked = true;
            }
            return Ok(());
        }

        if self.is_root(path) {
            self.root.devpath = Some(path.to_string());
            return Ok(());
        }

        let mut blkid_cache = get_blkid_cache();
        blkid_cache.probe_all_new()?;

        let filesystem = blkid_cache.get_tag_value("TYPE", &PathBuf::from(path));
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

    let passphrase = match &encrypted_device.unlock {
        UnlockType::Key(key) => {
            fs::read(key).with_context(|| format!("unable to read keyfile {:?}", key))?
        }
        UnlockType::AskPassphrase => ask_passphrase_for_device(encrypted_device)?.into_bytes(),
    };

    device.activate_handle().activate_by_passphrase(
        Some(&encrypted_device.name),
        None,
        &passphrase,
        CryptActivate::empty(),
    )?;

    Ok(())
}

pub(crate) fn parse_crypttab(crypttab_path: &str) -> Result<Vec<EncryptedDevice>> {
    let file =
        File::open(crypttab_path).with_context(|| format!("unable to open {:?}", crypttab_path))?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.starts_with('#'))
        .filter(|line| !line.is_empty())
        .map(EncryptedDevice::from_line)
        .filter_map(|device| match device {
            Ok(device) => Some(device),
            Err(err) => {
                log::warn!("failed to parse crypttab line: {:?}", err);
                None
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_crypttab() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/crypttab");
        let devices = parse_crypttab(path).unwrap();
        assert_eq!(devices.len(), 2);

        assert_eq!(devices[0].name, "luks-home");
        assert!(matches!(
            &devices[0].identifier,
            Identifier::Uuid(u) if u == "11111111-2222-3333-4444-555555555555"
        ));
        assert!(matches!(devices[0].unlock, UnlockType::AskPassphrase));

        assert_eq!(devices[1].name, "luks-data");
        assert!(matches!(
            &devices[1].identifier,
            Identifier::Path(p) if p == "/dev/sdb1"
        ));
        assert!(matches!(&devices[1].unlock, UnlockType::Key(k) if k == "/etc/keys/data.key"));
    }

    #[test]
    fn test_parse_crypttab_missing_file() {
        assert!(parse_crypttab("/nonexistent/path/crypttab").is_err());
    }

    fn make_cmdline(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn test_rd_luks_uuid_default_name() {
        let cmdline =
            make_cmdline("root=UUID=aaa rd.luks.uuid=11111111-2222-3333-4444-555555555555");
        let handler = DeviceHandler::init("/nonexistent/crypttab", &cmdline).unwrap();
        assert_eq!(handler.encrypted_devices.len(), 1);
        assert_eq!(
            handler.encrypted_devices[0].name,
            "luks-11111111-2222-3333-4444-555555555555"
        );
        assert!(matches!(
            &handler.encrypted_devices[0].identifier,
            Identifier::Uuid(u) if u == "11111111-2222-3333-4444-555555555555"
        ));
    }

    #[test]
    fn test_rd_luks_name_override() {
        let cmdline = make_cmdline(
            "root=UUID=aaa rd.luks.uuid=dead-beef rd.luks.name=dead-beef:cryptroot",
        );
        let handler = DeviceHandler::init("/nonexistent/crypttab", &cmdline).unwrap();
        assert_eq!(handler.encrypted_devices[0].name, "cryptroot");
    }

    #[test]
    fn test_rd_luks_no_duplicate_with_crypttab() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/crypttab");
        // crypttab already has luks-home with this UUID; cmdline should not add a duplicate
        let cmdline = make_cmdline(
            "root=UUID=aaa rd.luks.uuid=11111111-2222-3333-4444-555555555555",
        );
        let handler = DeviceHandler::init(path, &cmdline).unwrap();
        let count = handler
            .encrypted_devices
            .iter()
            .filter(|d| {
                matches!(&d.identifier, Identifier::Uuid(u)
                    if u == "11111111-2222-3333-4444-555555555555")
            })
            .count();
        assert_eq!(count, 1, "should not duplicate device already in crypttab");
    }
}

fn ask_passphrase_for_device(encrypted_device: &EncryptedDevice) -> Result<String> {
    rpassword::prompt_password(format!(
        "Password for device {}: ",
        encrypted_device.identifier
    ))
    .context("unable to read password from stdin")
}
