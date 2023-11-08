use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use colored::Colorize;
use log::warn;
use rayon::prelude::*;

use crate::initramfs_type::InitramfsType;

fn is_module_needed(name: &str, path: &Utf8Path) -> bool {
    let path = match path.strip_prefix("kernel/") {
        Ok(path) => path.as_str(),
        Err(_) => {
            warn!("module {} is not supported", path.as_str().purple().bold());
            return false;
        }
    };

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
    if path.contains("sd_mod")
        || path.contains("sr_mod")
        || path.contains("usb_storage")
        || path.contains("firewire-sbp2")
        || path.contains("block")
        || path.contains("scsi")
        || path.contains("fusion")
        || path.contains("nvme")
        || path.contains("mmc")
        || path.contains("tifm_")
        || path.contains("virtio")
        || path.contains("drivers/ata/")
        || path.contains("drivers/usb/host/")
        || path.contains("drivers/usb/storage/")
        || path.contains("drivers/firewire/")
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
    kroot: &Utf8Path,
    additional_modules: Vec<String>,
) -> Result<Vec<Utf8PathBuf>> {
    let additional_modules = additional_modules.into_iter().collect::<HashSet<String>>();
    let modules = get_all_modules(kroot)?;

    Ok(match initramfs_type {
        InitramfsType::General => modules
            .par_iter()
            .filter(|(name, path)| {
                is_module_needed(name, path) || additional_modules.contains(name)
            })
            .map(|(_, path)| kroot.join(path))
            .collect::<Vec<Utf8PathBuf>>(),
        InitramfsType::Host => {
            let host_modules = get_host_modules()?.into_iter().collect::<HashSet<String>>();
            modules
                .par_iter()
                .filter(|(name, path)| {
                    (host_modules.contains(name) && is_module_needed(name, path))
                        || additional_modules.contains(name)
                })
                .map(|(_, path)| kroot.join(path))
                .collect::<Vec<Utf8PathBuf>>()
        }
    })
}

fn get_module_name(filename: &Utf8Path) -> Result<String> {
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

fn get_all_modules(kroot: &Utf8Path) -> Result<Vec<(String, Utf8PathBuf)>> {
    BufReader::new(
        File::open(kroot.join("modules.dep")).with_context(|| "unable to open modules.dep")?,
    )
    .lines()
    .map_while(Result::ok)
    .map(|line| -> Result<(String, Utf8PathBuf)> {
        let module_path = Utf8Path::new(
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
    .map_while(Result::ok)
    .filter_map(|line| {
        line.split_whitespace()
            .next()
            .map(|module| module.to_string())
    })
    .collect())
}
