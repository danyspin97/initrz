mod device_handler;
mod encrypted_device;
mod encryption_type;
mod filesystem;
mod identifier;
mod module_loader;
mod mounts;
mod root_device;
mod uevent_listener;
mod unlock_type;
mod utils;

use anyhow::{bail, Context, Result};
use dowser::Dowser;
use libc;
use log::{error, info};
use nix::unistd::chroot;
use rayon::prelude::*;
use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

use std::{
    convert::TryFrom,
    env, fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc::channel, Arc},
    thread,
};

use device_handler::DeviceHandler;
use module_loader::ModuleLoader;
use mounts::Mounts;
use uevent_listener::UeventListener;
use utils::get_blkid_cache;

// Copyright (c) 2015 Guillaume Gomez
// https://github.com/GuillaumeGomez/sysinfo/blob/master/src/linux/system.rs#L524
fn get_kernel_version() -> Result<String> {
    let mut raw = std::mem::MaybeUninit::<libc::utsname>::zeroed();

    if unsafe { libc::uname(raw.as_mut_ptr()) } == 0 {
        let info = unsafe { raw.assume_init() };

        let release = info
            .release
            .iter()
            .filter(|c| **c != 0)
            .map(|c| *c as u8 as char)
            .collect::<String>();

        Ok(release)
    } else {
        bail!("uname call failed")
    }
}

pub fn parse_cmdline() -> Result<Vec<String>> {
    Ok(String::from_utf8(fs::read("/proc/cmdline")?)?
        .split_whitespace()
        .collect::<Vec<&str>>()
        .iter()
        .map(|s| String::from(*s))
        .collect())
}

fn initrz() -> Result<()> {
    TermLogger::init(
        LevelFilter::Trace,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )?;

    info!("mounting special filesystems");
    let mounts = Arc::new(Mounts::with_default_mounts()?);

    info!("parsing command line");
    let cmdline = parse_cmdline()?;

    info!("creating internal objects");
    let module_loader = Arc::new(ModuleLoader::init(&get_kernel_version()?)?);
    let mut device_handler = DeviceHandler::init("/etc/crypttab.initramfs", &cmdline)?;
    let uevent_listener = UeventListener::init(module_loader.clone())?;

    info!("loading qemu modules");
    module_loader.load_module("virtio_blk")?;
    module_loader.load_module("virtio_pci")?;
    // module_loader.load_module("virtio_net")?;

    // info!("loading essential modules");
    // module_loader.load_module("usbhid")?;

    // module_loader.load_all_modules()?;

    info!("probing available devices");
    let mut cache = get_blkid_cache();
    cache.probe_all()?;
    cache.probe_all_removable()?;
    cache.put_cache();
    std::mem::drop(cache);

    info!("creating channels");
    let (tx, rx) = channel::<String>();

    info!("unlocking available devices");
    device_handler.unlock_available_devices()?;
    info!("searching for root");
    device_handler.search_root()?;

    info!("starting uevent listener thread");
    thread::spawn(move || uevent_listener.listen(tx));

    info!("traversing /sys modalias files");
    Vec::<PathBuf>::try_from(
        Dowser::filtered(|p: &Path| {
            p.file_name()
                .filter(|filename| filename.to_str().unwrap_or("") == "modalias")
                .is_some()
        })
        .with_path("/sys"),
    )?
    .par_iter()
    .filter_map(|modalias| modalias.to_str())
    .try_for_each(|modalias| module_loader.load_modalias(modalias))?;

    info!("receiving mount results");
    device_handler.listen(rx)?;

    info!("moving /new_root into /");
    // switch_root
    // https://github.com/mirror/busybox/blob/9ec836c033fc6e55e80f3309b3e05acdf09bb297/util-linux/switch_root.c#L297
    mounts.mount_root(
        device_handler
            .get_root()
            .with_context(|| "unable to find root device")?,
        &module_loader,
    )?;

    info!("chrooting");
    chroot(".").with_context(|| "unable to chroot")?;
    env::set_current_dir("/")?;
    Command::new("/sbin/init").exec();

    Ok(())
}

fn main() {
    if let Err(err) = initrz() {
        error!("{:?}", err);
        Command::new("busybox").arg("sh").exec();
    }
}
