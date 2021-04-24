use std::{
    collections::HashSet,
    convert::TryFrom,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use dowser::Dowser;
use log::warn;
use rayon::prelude::*;

use crate::initramfs_type::InitramfsType;
use common::Modules;

fn is_general_module(path: &Path, filename: &str) -> bool {
    let path_str = path.as_os_str().to_str().unwrap();
    warn!("{}", path_str);
    // https://github.com/distr1/distri/blob/master/cmd/distri/initrd.go#L45
    if path.starts_with("fs") && !path.starts_with("fs/nls") {
        return true; // file systems
    }
    if path.starts_with("crypto")
        || filename == "dm-crypt.ko.xz"
        || filename == "dm-integrity.ko.xz"
    {
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

fn insert_module<'a>(
    module_name: &'a str,
    modules: &'a Modules,
    modules_added: &mut HashSet<&'a str>,
) {
    let module = modules.get(module_name).unwrap();
    modules_added.insert(module_name);
    module
        .deps
        .iter()
        .for_each(|dep| insert_module(dep, modules, modules_added))
}

pub fn get_modules(
    initramfs_type: &InitramfsType,
    kroot: &Path,
    additional_modules: Vec<String>,
) -> Result<Vec<PathBuf>> {
    let modules = Modules::new(&kroot)?;
    let mut modules_to_add = match initramfs_type {
        InitramfsType::General => {
            let mut modules_added: HashSet<&str> = HashSet::new();
            let general_modules = get_general_modules(kroot)?;
            // Add dependencies for each module
            general_modules
                .iter()
                .for_each(|module| insert_module(module, &modules, &mut modules_added));
            general_modules
        }
        // We assume dependencies are already loaded when considering host modules
        InitramfsType::Host => get_host_modules()?,
    };
    // Do not mind about replicated modules, Initramfs will handle those
    modules_to_add.extend(additional_modules);
    Ok(modules_to_add
        .par_iter()
        .filter_map(|module_name| {
            if let Some(module) = modules.get(module_name) {
                Some(kroot.join(&module.filename))
            } else {
                None
            }
        })
        .collect::<Vec<PathBuf>>())
}

fn get_general_modules(kroot: &Path) -> Result<Vec<String>> {
    let modules_root = kroot.join("kernel/");
    Ok(Vec::<PathBuf>::try_from(
        Dowser::filtered(move |p: &Path| {
            if !is_module(p) {
                return false;
            }
            let path = p.strip_prefix(&modules_root);
            if path.is_err() {
                return false;
            }
            let path = path.unwrap();
            let filename = path.file_name().unwrap().to_str().unwrap();
            is_general_module(path, filename)
        })
        .with_path(kroot.join("kernel")),
    )?
    .iter()
    .map(|path| get_module_name(path))
    .collect::<Result<Vec<_>>>()?)
}

fn get_module_name(filename: &PathBuf) -> Result<String> {
    Ok(filename
        .file_stem()
        .and_then(|module| std::path::Path::new(module).file_stem())
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
