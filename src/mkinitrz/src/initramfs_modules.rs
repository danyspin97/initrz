use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::initramfs_type::InitramfsType;

fn is_module_needed(name: &str, path: &Path) -> bool {
    let path = path
        .strip_prefix("kernel/")
        .expect("expected filename starting with 'kernel', malformed modules.dep");
    let path_str = path.as_os_str().to_str().unwrap();
    // https://github.com/distr1/distri/blob/master/cmd/distri/initrd.go#L45
    if path.starts_with("fs") && !path.starts_with("fs/nls") {
        return true; // file systems
    }
    if path.starts_with("crypto") || name == "dm-crypt" || name == "dm-integrity" {
        return true; // disk encryption
    }
    if path.starts_with("drivers/md/") || path.starts_with("lib/") {
        return true; // device mapper
    }
    if path_str.contains("sd_mod")
        || path_str.contains("sr_mod")
        || path_str.contains("usb_storage")
        || path_str.contains("firewire-sbp2")
        || path_str.contains("block")
        || path_str.contains("scsi")
        || path_str.contains("fusion")
        || path_str.contains("nvme")
        || path_str.contains("mmc")
        || path_str.contains("tifm_")
        || path_str.contains("virtio")
        || path_str.contains("drivers/ata/")
        || path_str.contains("drivers/usb/host/")
        || path_str.contains("drivers/usb/storage/")
        || path_str.contains("drivers/firewire/")
    {
        return true; // block devices
    }
    if path.starts_with("drivers/hid/")
        || path.starts_with("drivers/input/keyboard/")
        || path.starts_with("drivers/input/serio/")
        || path.starts_with("usbhid")
    {
        return true; // keyboard input
    }

    false
}

pub fn get_modules(
    initramfs_type: InitramfsType,
    kroot: &Path,
    additional_modules: Vec<String>,
) -> Result<Vec<PathBuf>> {
    let additional_modules = additional_modules.into_iter().collect::<HashSet<String>>();
    let modules = get_all_modules(kroot)?;

    Ok(match initramfs_type {
        InitramfsType::General => modules
            .par_iter()
            .filter(|(name, path)| {
                is_module_needed(name, path) || additional_modules.contains(name)
            })
            .map(|(_, path)| kroot.join(path))
            .collect::<Vec<PathBuf>>(),
        InitramfsType::Host => {
            let host_modules = get_host_modules()?.into_iter().collect::<HashSet<String>>();
            modules
                .par_iter()
                .filter(|(name, path)| {
                    (host_modules.contains(name) && is_module_needed(name, path))
                        || additional_modules.contains(name)
                })
                .map(|(_, path)| kroot.join(path))
                .collect::<Vec<PathBuf>>()
        }
    })
}

fn get_module_name(filename: &Path) -> Result<String> {
    Ok(filename
        .file_stem()
        .and_then(|module| Path::new(module).file_stem())
        .with_context(|| format!("failed to get module name of file {:?}", filename))?
        .to_str()
        .with_context(|| {
            format!(
                "failed to convert the module name in file {:?} from OsStr to Str",
                filename
            )
        })?
        .to_string())
}
fn is_module(p: &Path) -> bool {
    p.extension()
        .filter(|ext| ext.to_str().unwrap_or("") == "xz")
        .is_some()
        && p.file_stem()
            .filter(|stem| {
                Path::new(stem)
                    .extension()
                    .filter(|ext| ext.to_str().unwrap_or("") == "ko")
                    .is_some()
            })
            .is_some()
}

fn get_all_modules(kroot: &Path) -> Result<Vec<(String, PathBuf)>> {
    BufReader::new(
        File::open(kroot.join("modules.dep")).with_context(|| "unable to open modules.dep")?,
    )
    .lines()
    .filter_map(|line| line.ok())
    .map(|line| -> Result<(String, PathBuf)> {
        let module_path = Path::new(
            line.split(':')
                .next()
                .with_context(|| "unable to get module from modules.dep")?,
        );
        Ok((get_module_name(module_path)?, module_path.to_path_buf()))
    })
    .collect()
}

fn get_host_modules() -> Result<Vec<String>> {
    Ok(BufReader::new(
        File::open("/proc/modules").with_context(|| "unable to open file /proc/modules")?,
    )
    .lines()
    .filter_map(|line| line.ok())
    .filter_map(|line| {
        line.split_whitespace()
            .next()
            .and_then(|module| Some(module.to_string()))
    })
    .collect())
}
