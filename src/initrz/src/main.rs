mod device_handler;
mod module_loader;
mod uevent_listener;
mod utils;

use anyhow::{bail, Context, Result};
use dowser::Dowser;
use libc;
use log::{error, info, trace};
use mount_api::Fs;
use mount_api::FsmountFlags;
use mount_api::FsopenFlags;
use mount_api::Mount;
use mount_api::MountAttrFlags;
use mount_api::MoveMountFlags;
use nix::unistd::chroot;
use rayon::prelude::*;
use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

use std::convert::TryFrom;
use std::env;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc::channel, Arc, Mutex};
use std::thread;
use std::time::Duration;

use device_handler::DeviceHandler;
use module_loader::ModuleLoader;
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

fn mount_special_filesystem(parent_dir: RawFd, mount_folder: &str, fs_name: &str) -> Result<Mount> {
    let fs = Fs::open(&CString::new(fs_name)?, FsopenFlags::empty())
        .with_context(|| format!("failed to open a filesystem context of type {}", fs_name))?;
    fs.create()
        .with_context(|| format!("unable to create filesystem context for type {}", fs_name))?;
    let mount = fs.mount(FsmountFlags::empty(), MountAttrFlags::empty())?;

    fs::create_dir_all(Path::new(mount_folder))
        .with_context(|| format!("unable to create directory {:?}", mount_folder))?;
    mount.move_mount(parent_dir, mount_folder, MoveMountFlags::empty())?;
    Ok(mount)
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

    let root_dir = File::open("/").with_context(|| format!("failed to open root directory"))?;

    info!("mounting /dev");
    let dev_mount = mount_special_filesystem(root_dir.as_raw_fd(), "dev", "devtmpfs")
        .with_context(|| format!("unable to mount /dev"))?;
    info!("mounting /sys");
    let sys_mount = mount_special_filesystem(root_dir.as_raw_fd(), "sys", "sysfs")
        .with_context(|| format!("unable to mount /sys"))?;
    info!("mounting /proc");
    let proc_mount = mount_special_filesystem(root_dir.as_raw_fd(), "proc", "proc")
        .with_context(|| format!("unable to mount /proc"))?;

    info!("parsing command line");
    let cmdline = parse_cmdline()?;

    info!("creating internal objects");
    let module_loader = Arc::new(ModuleLoader::init(&get_kernel_version()?)?);
    let device_handler = Arc::new(Mutex::new(DeviceHandler::init("/crypttab", &cmdline)?));
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
    let (device_tx, device_rx) = channel::<String>();
    let (res_tx, res_rx) = channel::<Result<()>>();

    let mut guard = device_handler.lock().unwrap();
    info!("unlocking available devices");
    guard.unlock_available_devices()?;
    info!("searching for root");
    guard.search_root()?;
    std::mem::drop(guard);

    info!("starting uevent listener thread");
    thread::spawn(move || uevent_listener.listen(device_tx));
    let device_handler_clone = device_handler.clone();
    info!("starting device mounter thread");
    thread::spawn(move || {
        device_handler_clone
            .lock()
            .unwrap()
            .listen(device_rx, res_tx)
    });

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
    loop {
        let res = res_rx.try_recv();
        if res.is_err() {
            match res.unwrap_err() {
                std::sync::mpsc::TryRecvError::Disconnected => break,
                _ => {}
            }
        } else {
            res.unwrap()?;
        }
    }

    info!("moving /new_root into /");
    // switch_root
    // https://github.com/mirror/busybox/blob/9ec836c033fc6e55e80f3309b3e05acdf09bb297/util-linux/switch_root.c#L297
    env::set_current_dir("/new_root")?;
    device_handler.lock().unwrap().move_root_mount()?;
    info!("chrooting");
    chroot(".").with_context(|| "unable to chroot")?;

    env::set_current_dir("/")?;

    Command::new("/sbin/init").exec();

    Ok(())
}

fn main() {
    if let Err(err) = initrz() {
        error!("{:?}", err);
        Command::new("ash").exec();
    }
}
