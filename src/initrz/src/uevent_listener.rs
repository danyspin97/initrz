use anyhow::{bail, Context, Result};
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
            .with_context(|| format!("unable to create socket"))?;
        // socket
        //     .set_non_blocking(true)
        //     .with_context(|| format!("unable to set O_NONBLOCK to socket"))?;
        let kernel_addr = SocketAddr::new(0, 1);
        // socket
        //     .connect(&kernel_addr)
        //     .with_context(|| format!("unable to connect to kernel address"))?;
        socket
            .bind(&kernel_addr)
            .with_context(|| format!("unable to bind socket"))?;

        Ok(UeventListener {
            socket,
            module_loader,
        })
    }

    pub fn listen(&self, device_tx: Sender<String>) {
        loop {
            let mut buf = vec![0; 4096];
            let msg = self.socket.recv(&mut buf, 0);
            if msg.is_err() {
                // warn!("unable to receive from socket");
                continue;
            }
            let msglen = msg.unwrap();
            if msglen == 0 {
                // received empty message
                continue;
            }
            let uevent = parse_uevent(&buf[0..msglen - 1]);
            if uevent.is_err() {
                warn!("uevent: {:?}", uevent);
                continue;
            }

            let uevent = uevent.unwrap();
            let res = self.get_device_path(uevent);
            if res.is_err() {
                warn!("uevent: {:?}", res);
            } else if let Some(path) = res.unwrap() {
                if let Err(err) = device_tx.send(path) {
                    warn!("send error: {:?}", err);
                    break;
                }
            }
        }
    }

    fn get_device_path(&self, uevent: Uevent) -> Result<Option<String>> {
        let modalias = uevent.vars.get("MODALIAS");
        if modalias.is_some() {
            self.module_loader.load_modalias(modalias.unwrap())?;
        }

        let devpath = uevent
            .vars
            .get("DEVPATH")
            .with_context(|| "unable to find DEVPATH in uevent")?;
        let devname = Path::new(devpath)
            .file_name()
            .with_context(|| format!("unable to get filename from DEVPATH {}", devpath))?
            .to_str()
            .with_context(|| "unable to convert OsString to String")?;
        let action = uevent
            .vars
            .get("ACTION")
            .with_context(|| "unable to find ACTION in uevent")?;

        let subsystem = uevent
            .vars
            .get("SUBSYSTEM")
            .with_context(|| "unable to find SUBSYSTEM in uevent")?;

        // (
        //     devname is not dm-* and action is add
        // ||
        //     is devname is dm-* and action is change
        // )
        // and
        // subsytem is block

        if subsystem != "block" {
            return Ok(None);
        }

        if devname.starts_with("dm-") && action != "change" {
            return Ok(None);
        }

        if !devname.starts_with("dm-") && action != "add" {
            return Ok(None);
        }

        Ok(Some(
            if devname.starts_with("dm-") {
                let dm_name = Path::new("/sys").join(devname).join("dm/name");
                if !dm_name.exists() {
                    bail!("unable to find file {:?}", dm_name);
                }
                let dm_name = String::from_utf8(fs::read(dm_name)?)?;
                Path::new("/dev/mapper").join(dm_name)
            } else {
                Path::new("/dev").join(devname)
            }
            .to_string_lossy()
            .to_string(),
        ))
    }
}

pub fn parse_uevent(buf: &[u8]) -> Result<Uevent> {
    let mut lines = buf.split(|c| c == &0);

    let name = lines
        .next()
        .with_context(|| format!("uncorrect uevent received"))?;
    name.find_char('@')
        .with_context(|| format!("uncorrect uevent received"))?;

    let vars: HashMap<_, _> = lines
        .into_iter()
        .map(|line| parse_line(line))
        .filter_map(|res| {
            if res.is_err() {
                warn!("unable to process line\n{:?}", res);
            }
            res.ok()
        })
        .collect();

    Ok(Uevent {
        name: CString::new(name)?.into_string()?,
        vars,
    })
}

pub fn parse_line(line: &[u8]) -> Result<(String, String)> {
    let token_index = line.find_char('=').with_context(|| {
        format!(
            "unable to locate '=' in line '{}'",
            String::from_utf8_lossy(line)
        )
    })?;
    Ok((
        CString::new(&line[0..token_index])?.into_string()?,
        CString::new(&line[token_index + 1..])?.into_string()?,
    ))
}
