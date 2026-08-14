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

use anyhow::{Context, Result};
use dowser::Dowser;
use log::{error, info};
use nix::unistd::chroot;
use rayon::prelude::*;
use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

use std::{
    env, fs,
    os::unix::process::CommandExt,
    path::Path,
    process::Command,
    sync::Arc,
    thread,
    time::Duration,
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
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8 as char)
            .collect();
        Ok(release)
    } else {
        anyhow::bail!("uname() failed")
    }
}

pub fn parse_cmdline() -> Result<Vec<String>> {
    Ok(String::from_utf8(fs::read("/proc/cmdline")?)?
        .split_whitespace()
        .map(str::to_string)
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

    info!("probing available devices");
    let mut cache = get_blkid_cache();
    cache.probe_all()?;
    cache.probe_all_removable()?;
    cache.put_cache();
    std::mem::drop(cache);

    info!("unlocking available encrypted devices");
    device_handler.unlock_available_devices()?;

    info!("searching for root device");
    device_handler.search_root()?;

    info!("starting uevent listener thread");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    thread::spawn(move || uevent_listener.listen(tx));

    info!("traversing /sys modalias files to load modules");
    Dowser::default()
        .with_path("/sys")
        .into_vec_filtered(|p: &Path| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "modalias")
                .unwrap_or(false)
        })
        .par_iter()
        .filter_map(|modalias| modalias.to_str())
        .try_for_each(|modalias| module_loader.load_modalias(modalias))?;

    let rootwait_secs = cmdline
        .iter()
        .filter_map(|arg| arg.strip_prefix("rootwait="))
        .filter_map(|val| val.parse::<u64>().ok())
        .last()
        .unwrap_or(30);

    info!("waiting for root device (timeout {}s)", rootwait_secs);
    device_handler.listen(rx, Duration::from_secs(rootwait_secs))?;

    info!("mounting root");
    mounts.mount_root(
        device_handler
            .get_root()
            .with_context(|| "unable to find root device")?,
        &module_loader,
    )?;

    info!("switching root");
    chroot(".").with_context(|| "chroot failed")?;
    env::set_current_dir("/")?;
    let err = Command::new("/sbin/init").exec();
    Err(err).with_context(|| "failed to exec /sbin/init")
}

fn main() {
    if let Err(err) = initrz() {
        error!("{:?}", err);
        let err = Command::new("/bin/busybox").arg("sh").exec();
        error!("failed to exec rescue shell: {:?}", err);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cmdline_extracts_root() {
        // parse_cmdline reads /proc/cmdline from the real system — just verify it doesn't panic
        // and returns a non-empty vec on a running system.
        if Path::new("/proc/cmdline").exists() {
            let args = parse_cmdline().unwrap();
            assert!(!args.is_empty());
        }
    }
}
