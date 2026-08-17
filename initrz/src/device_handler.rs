use anyhow::{Context, Result};
use libblkid_rs::BlkidCache;
use libcryptsetup_rs::consts::flags::CryptActivate;
use libcryptsetup_rs::CryptInit;

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use crate::encrypted_device::EncryptedDevice;
use crate::encryption_type::EncryptionType;
use crate::identifier::Identifier;
use crate::module_loader::ModuleLoader;
use crate::resume;
use crate::root_device::{get_root_from_cmdline, RootDevice};
use crate::unlock_type::UnlockType;
use crate::utils::get_blkid_cache;

const UUID_TAG: &str = "UUID";

struct ResumeConfig {
    identifier: Identifier,
    offset: Option<u64>,
    devpath: Option<String>,
}

pub struct DeviceHandler {
    root: RootDevice,
    resume: Option<ResumeConfig>,
    pub(crate) encrypted_devices: Vec<EncryptedDevice>,
    module_loader: Arc<ModuleLoader>,
    // Held open for the handler's whole lifetime instead of being reopened (and the on-disk
    // cache file re-parsed) on every lookup: DeviceHandler's methods run repeatedly on the
    // boot-critical path (once per uevent), so that cost would otherwise be paid over and over.
    blkid_cache: BlkidCache,
}

/// dm-crypt (LUKS) and device-mapper LVM activation both need the dm_mod module, but neither
/// has a PCI/USB modalias to trigger the usual sysfs-scan autoload - it has to be requested
/// explicitly. load_module() is idempotent and dependency-resolving, so calling this from every
/// DM-dependent call site is cheap and safe.
fn ensure_dm_mod_loaded(module_loader: &ModuleLoader) {
    // ModuleLoader keys modules by filename stem (hyphenated, "dm-mod"), not the canonical
    // in-kernel module name reported by /proc/modules (underscored, "dm_mod").
    if let Err(err) = module_loader.load_module("dm-mod") {
        log::warn!("unable to load dm-mod: {:?}", err);
    }
}

impl DeviceHandler {
    pub fn init(
        crypttab_path: &str,
        cmdline: &[String],
        module_loader: Arc<ModuleLoader>,
    ) -> Result<DeviceHandler> {
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

        // resume=<UUID=...|path> selects a device to look for a hibernation image on;
        // resume_offset=<bytes> is needed for swap-file resume. noresume disables resume
        // entirely even if resume= is present, matching the kernel/dracut convention.
        let noresume = cmdline.iter().any(|arg| arg == "noresume");
        let resume = if noresume {
            None
        } else {
            cmdline
                .iter()
                .filter_map(|arg| arg.strip_prefix("resume="))
                .last()
                .map(|identifier| ResumeConfig {
                    identifier: Identifier::from(identifier),
                    offset: cmdline
                        .iter()
                        .filter_map(|arg| arg.strip_prefix("resume_offset="))
                        .filter_map(|val| val.parse::<u64>().ok())
                        .last(),
                    devpath: None,
                })
        };

        Ok(DeviceHandler {
            root: get_root_from_cmdline(cmdline)?,
            resume,
            encrypted_devices,
            module_loader,
            blkid_cache: get_blkid_cache()?,
        })
    }

    fn is_ready(&self) -> bool {
        let all_unlocked = self.encrypted_devices.iter().all(|d| d.unlocked);
        let resume_ready = self.resume.as_ref().map_or(true, |r| r.devpath.is_some());
        self.root.devpath.is_some() && all_unlocked && resume_ready
    }

    pub fn listen(&mut self, device_rx: Receiver<String>, timeout: Duration) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }
        // timeout comes from the unvalidated rootwait= cmdline parameter; a huge value must not
        // panic (Instant + Duration overflow aborts the process, and we're PID 1). Fall back to
        // the furthest representable deadline instead, which still lets recv_timeout() work with
        // "remaining" durations far larger than any real boot would ever wait for.
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(365 * 24 * 3600));
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
        let uuid = self
            .blkid_cache
            .get_tag_value("UUID", &PathBuf::from(path))
            .unwrap_or_default();
        // Try path match first, then UUID match
        if let Some(pos) = self
            .encrypted_devices
            .iter()
            .position(|d| match &d.identifier {
                Identifier::Path(saved_path) => saved_path == path,
                Identifier::Uuid(saved_uuid) => saved_uuid == &uuid,
            })
        {
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

    fn is_resume(&self, devname: &str) -> bool {
        self.resume
            .as_ref()
            .filter(|r| r.devpath.is_none())
            .and_then(|r| r.identifier.get_path().ok())
            .map(|path| path == devname)
            .unwrap_or(false)
    }

    pub fn search_resume(&mut self) -> Result<bool> {
        let identifier = match &self.resume {
            Some(r) if r.devpath.is_none() => &r.identifier,
            _ => return Ok(false),
        };

        let mut found = None;
        for device in self.blkid_cache.iter() {
            let devname = device.devname()?;
            let matched = match identifier {
                Identifier::Uuid(uuid) => device
                    .tag_iter()
                    .any(|tag| tag == (String::from(UUID_TAG), String::from(uuid))),
                Identifier::Path(path) => devname.to_str().unwrap() == path,
            };
            if matched {
                found = Some(devname.to_str().unwrap().to_string());
                break;
            }
        }

        let Some(devpath) = found else {
            return Ok(false);
        };
        self.mark_resume_found(devpath)?;
        Ok(true)
    }

    fn mark_resume_found(&mut self, devpath: String) -> Result<()> {
        let resume = self.resume.as_mut().expect("resume is set");
        resume::try_resume(&devpath, resume.offset)?;
        resume.devpath = Some(devpath);
        Ok(())
    }

    pub fn search_root(&mut self) -> Result<bool> {
        // A root= pointing directly at a regular file (e.g. a squashfs image baked into the
        // initramfs) is available immediately: it isn't a block device, so no uevent will ever
        // announce it, and blkid can't be used to resolve it either.
        if let Identifier::Path(path) = &self.root.identifier {
            if Path::new(path).is_file() {
                self.root.devpath = Some(path.clone());
                return Ok(true);
            }
        }

        for device in self.blkid_cache.iter() {
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
        for device in self.blkid_cache.iter() {
            let devname = device.devname()?;
            let devname_str = devname.to_str().unwrap();
            if let Some(pos) = self
                .encrypted_devices
                .iter()
                .position(|enc| match &enc.identifier {
                    Identifier::Uuid(uuid) => device
                        .tag_iter()
                        .any(|tag| tag == (String::from(UUID_TAG), String::from(uuid))),
                    Identifier::Path(path) => devname_str == path,
                })
            {
                ensure_dm_mod_loaded(&self.module_loader);
                unlock_luks_device(devname_str, &self.encrypted_devices[pos])?;
                self.encrypted_devices[pos].unlocked = true;
            }
        }

        Ok(())
    }

    pub fn handle(&mut self, path: &str) -> Result<()> {
        let module_loader = self.module_loader.clone();
        if let Some(encrypted_device) = self.get_encrypted_device_mut(path) {
            if !encrypted_device.unlocked {
                ensure_dm_mod_loaded(&module_loader);
                unlock_luks_device(path, encrypted_device)?;
                encrypted_device.unlocked = true;
            }
            return Ok(());
        }

        if self.is_root(path) {
            self.root.devpath = Some(path.to_string());
            return Ok(());
        }

        if self.is_resume(path) {
            self.mark_resume_found(path.to_string())?;
            return Ok(());
        }

        self.blkid_cache.probe_all_new()?;

        let filesystem = self.blkid_cache.get_tag_value("TYPE", &PathBuf::from(path));
        if filesystem.is_err() {
            // We have got a block device with no filesystem, skip
            return Ok(());
        }
        let filesystem = filesystem.unwrap();
        // blkid reports LVM physical volumes as "LVM2_member" (matching `blkid`/`lsblk -f`
        // output), never "lvm" - this comparison predates lvm.rs and never actually matched
        // anything on a real system.
        if filesystem == "LVM2_member" {
            ensure_dm_mod_loaded(&self.module_loader);
            crate::lvm::activate_volume_group(path)
                .with_context(|| format!("activating volume group on {:?}", path))?;
        }

        self.blkid_cache.put_cache();
        Ok(())
    }
}

/// Number of times to re-prompt for a passphrase before giving up on a device. A keyfile is
/// never retried: if it's wrong once, re-reading the same file won't change that.
const MAX_PASSPHRASE_ATTEMPTS: u32 = 3;

fn unlock_luks_device(path: &str, encrypted_device: &EncryptedDevice) -> Result<()> {
    let mut device = CryptInit::init(Path::new(path))?;
    // Passing None for the format lets libcryptsetup auto-detect LUKS1 vs LUKS2 (and any other
    // format it recognizes) from the on-disk header, instead of assuming every volume is LUKS2 -
    // crypttab's "luks" type (EncryptionType::Luks) covers both versions, matching dracut/systemd
    // behavior of not hardcoding a version.
    device.context_handle().load::<()>(None, None)?;

    match &encrypted_device.unlock {
        UnlockType::Key(key) => {
            let passphrase =
                fs::read(key).with_context(|| format!("unable to read keyfile {:?}", key))?;
            device.activate_handle().activate_by_passphrase(
                Some(&encrypted_device.name),
                None,
                &passphrase,
                CryptActivate::empty(),
            )?;
        }
        UnlockType::AskPassphrase => {
            for attempt in 1..=MAX_PASSPHRASE_ATTEMPTS {
                let passphrase = ask_passphrase_for_device(encrypted_device)?.into_bytes();
                match device.activate_handle().activate_by_passphrase(
                    Some(&encrypted_device.name),
                    None,
                    &passphrase,
                    CryptActivate::empty(),
                ) {
                    Ok(_) => break,
                    Err(err) if attempt < MAX_PASSPHRASE_ATTEMPTS => {
                        log::warn!(
                            "wrong passphrase for device {} (attempt {}/{}): {}",
                            encrypted_device.identifier,
                            attempt,
                            MAX_PASSPHRASE_ATTEMPTS,
                            err
                        );
                    }
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!(
                                "failed to unlock device {} after {} attempts",
                                encrypted_device.identifier, MAX_PASSPHRASE_ATTEMPTS
                            )
                        })
                    }
                }
            }
        }
    }

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
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
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
        let cmdline =
            make_cmdline("root=UUID=aaa rd.luks.uuid=dead-beef rd.luks.name=dead-beef:cryptroot");
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        assert_eq!(handler.encrypted_devices[0].name, "cryptroot");
    }

    #[test]
    fn test_rd_luks_no_duplicate_with_crypttab() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/crypttab");
        // crypttab already has luks-home with this UUID; cmdline should not add a duplicate
        let cmdline =
            make_cmdline("root=UUID=aaa rd.luks.uuid=11111111-2222-3333-4444-555555555555");
        let handler = DeviceHandler::init(path, &cmdline, Arc::new(ModuleLoader::empty())).unwrap();
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

    #[test]
    fn test_resume_uuid_parsed() {
        let cmdline = make_cmdline("root=UUID=aaa resume=UUID=deadbeef-0000");
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        let resume = handler.resume.as_ref().unwrap();
        assert!(matches!(&resume.identifier, Identifier::Uuid(u) if u == "deadbeef-0000"));
        assert_eq!(resume.offset, None);
    }

    #[test]
    fn test_resume_offset_parsed() {
        let cmdline = make_cmdline("root=UUID=aaa resume=/dev/sda2 resume_offset=12345");
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        let resume = handler.resume.as_ref().unwrap();
        assert!(matches!(&resume.identifier, Identifier::Path(p) if p == "/dev/sda2"));
        assert_eq!(resume.offset, Some(12345));
    }

    #[test]
    fn test_no_resume_by_default() {
        let cmdline = make_cmdline("root=UUID=aaa");
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        assert!(handler.resume.is_none());
    }

    #[test]
    fn test_listen_handles_huge_rootwait_without_panicking() {
        // rootwait= is an unvalidated u64 straight off the kernel cmdline; Instant::now() +
        // Duration::from_secs(u64::MAX) used to panic ("overflow when adding duration to
        // instant") before it ever got a chance to wait for anything. Dropping the sender makes
        // recv_timeout() return Disconnected immediately regardless of the timeout, so this test
        // exercises the deadline computation without actually blocking.
        let cmdline = make_cmdline("root=UUID=aaa");
        let mut handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        drop(tx);

        let err = handler
            .listen(rx, Duration::from_secs(u64::MAX))
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("uevent listener thread exited unexpectedly"));
    }

    #[test]
    fn test_noresume_disables_resume() {
        let cmdline = make_cmdline("root=UUID=aaa resume=UUID=deadbeef-0000 noresume");
        let handler = DeviceHandler::init(
            "/nonexistent/crypttab",
            &cmdline,
            Arc::new(ModuleLoader::empty()),
        )
        .unwrap();
        assert!(handler.resume.is_none());
    }
}

fn ask_passphrase_for_device(encrypted_device: &EncryptedDevice) -> Result<String> {
    rpassword::prompt_password(format!(
        "Password for device {}: ",
        encrypted_device.identifier
    ))
    .context("unable to read password from stdin")
}
