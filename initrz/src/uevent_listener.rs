use anyhow::{Context, Result};
use bstr::ByteSlice;
use log::warn;
use netlink_sys::{protocols::NETLINK_KOBJECT_UEVENT, Socket, SocketAddr};

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::module_loader::ModuleLoader;

#[derive(Debug)]
pub struct Uevent {
    name: String,
    vars: HashMap<String, String>,
}

pub struct UeventListener {
    socket: Socket,
    module_loader: Arc<ModuleLoader>,
}

impl UeventListener {
    pub fn init(module_loader: Arc<ModuleLoader>) -> Result<UeventListener> {
        let mut socket = Socket::new(NETLINK_KOBJECT_UEVENT)
            .with_context(|| "unable to create netlink uevent socket")?;
        let kernel_addr = SocketAddr::new(0, 1);
        socket
            .bind(&kernel_addr)
            .with_context(|| "unable to bind netlink socket to kernel address")?;

        Ok(UeventListener {
            socket,
            module_loader,
        })
    }

    pub fn listen(&self, device_tx: Sender<String>) {
        // `Socket::recv` takes a `bytes::BufMut` and writes into its *spare capacity past the
        // current length* (then grows the length to cover what was written) - it does not fill
        // the buffer from index 0. A fixed-size `vec![0u8; 4096]` already has len == capacity, so
        // each call would reallocate and write past index 4096, leaving `buf[..msglen]` as the
        // untouched zeroed prefix. Clearing (not resizing) `buf` before each recv keeps its
        // capacity but resets len to 0, so the received bytes land at `buf[..msglen]` as expected.
        let mut buf = Vec::with_capacity(4096);
        loop {
            buf.clear();
            let msglen = match self.socket.recv(&mut buf, 0) {
                Ok(n) if n == 0 => continue,
                Ok(n) => n,
                Err(e) => {
                    warn!("uevent socket recv error: {}", e);
                    continue;
                }
            };

            let uevent = match parse_uevent(&buf[..msglen]) {
                Ok(u) => u,
                Err(e) => {
                    warn!("failed to parse uevent: {:?}", e);
                    continue;
                }
            };

            match self.get_device_path(uevent) {
                Ok(Some(path)) => {
                    if device_tx.send(path).is_err() {
                        // Receiver dropped; main thread found root, exit cleanly
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("uevent device path error: {:?}", e),
            }
        }
    }

    fn get_device_path(&self, uevent: Uevent) -> Result<Option<String>> {
        if let Some(modalias) = uevent.vars.get("MODALIAS") {
            self.module_loader.load_modalias(modalias)?;
        }

        let devpath = uevent
            .vars
            .get("DEVPATH")
            .with_context(|| "missing DEVPATH in uevent")?;
        let devname = Path::new(devpath)
            .file_name()
            .with_context(|| format!("unable to get filename from DEVPATH {}", devpath))?
            .to_str()
            .with_context(|| "DEVPATH filename is not valid UTF-8")?;
        let action = uevent
            .vars
            .get("ACTION")
            .with_context(|| "missing ACTION in uevent")?;
        let subsystem = uevent
            .vars
            .get("SUBSYSTEM")
            .with_context(|| "missing SUBSYSTEM in uevent")?;

        if subsystem != "block" {
            return Ok(None);
        }

        // dm-* devices trigger on "change", regular block devices on "add"
        if devname.starts_with("dm-") && action != "change" {
            return Ok(None);
        }
        if !devname.starts_with("dm-") && action != "add" {
            return Ok(None);
        }

        let path = if devname.starts_with("dm-") {
            let dm_name_path = Path::new("/sys/block").join(devname).join("dm/name");
            let dm_name = fs::read_to_string(&dm_name_path)
                .with_context(|| format!("unable to read {:?}", dm_name_path))?;
            let path = Path::new("/dev/mapper").join(dm_name.trim());
            // Unlike plain block devices (which devtmpfs already creates at /dev/<name>
            // automatically), the kernel only ever creates /dev/<devname> (e.g. /dev/dm-0) for
            // device-mapper devices - the friendly /dev/mapper/<name> path is normally created
            // by udev, which doesn't run here, so we have to mknod it ourselves.
            mknod_block_device(devname, &path)?;
            path
        } else {
            Path::new("/dev").join(devname)
        };

        Ok(Some(path.to_string_lossy().into_owned()))
    }
}

/// Create a block device node at `path` with the same major:minor as `/sys/block/<devname>`.
fn mknod_block_device(devname: &str, path: &Path) -> Result<()> {
    let dev_path = Path::new("/sys/block").join(devname).join("dev");
    let dev_str =
        fs::read_to_string(&dev_path).with_context(|| format!("unable to read {:?}", dev_path))?;
    let (major, minor) = dev_str
        .trim()
        .split_once(':')
        .with_context(|| format!("unexpected content in {:?}: {:?}", dev_path, dev_str))?;
    let major: u64 = major
        .parse()
        .with_context(|| format!("parsing major from {:?}", dev_str))?;
    let minor: u64 = minor
        .parse()
        .with_context(|| format!("parsing minor from {:?}", dev_str))?;
    // Matches glibc's gnu_dev_makedev() (bits/sysmacros.h), the inverse of the encoding used to
    // decode st_rdev elsewhere in this codebase (see lvm.rs's major_minor()).
    let dev = ((major & 0xfffff000) << 32)
        | ((major & 0xfff) << 8)
        | ((minor & 0xffffff00) << 12)
        | (minor & 0xff);

    match nix::sys::stat::mknod(
        path,
        nix::sys::stat::SFlag::S_IFBLK,
        nix::sys::stat::Mode::from_bits_truncate(0o600),
        dev,
    ) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("mknod failed for {:?}", path)),
    }
}

pub fn parse_uevent(buf: &[u8]) -> Result<Uevent> {
    // Strip trailing null byte if present
    let buf = buf.strip_suffix(&[0]).unwrap_or(buf);
    let mut lines = buf.split(|c| *c == 0);

    let name_bytes = lines.next().with_context(|| "empty uevent buffer")?;
    name_bytes
        .find_char('@')
        .with_context(|| "uevent header missing '@' separator")?;

    let vars = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_line(line) {
            Ok(kv) => Some(kv),
            Err(e) => {
                warn!("unable to parse uevent line: {:?}", e);
                None
            }
        })
        .collect();

    Ok(Uevent {
        name: CString::new(name_bytes)?.into_string()?,
        vars,
    })
}

pub fn parse_line(line: &[u8]) -> Result<(String, String)> {
    let token_index = line.find_char('=').with_context(|| {
        format!(
            "missing '=' in uevent line '{}'",
            String::from_utf8_lossy(line)
        )
    })?;
    Ok((
        CString::new(&line[..token_index])?.into_string()?,
        CString::new(&line[token_index + 1..])?.into_string()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_uevent_add() {
        let buf = b"add@/devices/pci0000:00/sda\0ACTION=add\0SUBSYSTEM=block\0DEVPATH=/devices/pci0000:00/sda\0";
        let uevent = parse_uevent(buf).unwrap();
        assert_eq!(uevent.vars["ACTION"], "add");
        assert_eq!(uevent.vars["SUBSYSTEM"], "block");
    }

    #[test]
    fn test_parse_line() {
        let line = b"ACTION=add";
        let (key, val) = parse_line(line).unwrap();
        assert_eq!(key, "ACTION");
        assert_eq!(val, "add");
    }

    #[test]
    fn test_parse_line_missing_equals() {
        let line = b"NOEQUALSSIGN";
        assert!(parse_line(line).is_err());
    }

    #[test]
    fn test_parse_uevent_missing_at() {
        let buf = b"invalid_header\0ACTION=add\0";
        assert!(parse_uevent(buf).is_err());
    }
}
