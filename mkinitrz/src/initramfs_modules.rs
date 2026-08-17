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

    let selected_names: HashSet<String> = match initramfs_type {
        InitramfsType::General => modules
            .par_iter()
            .filter(|(name, path, _deps)| {
                is_module_needed(name, path) || additional_modules.contains(name)
            })
            .map(|(name, _, _)| name.clone())
            .collect(),
        InitramfsType::Host => {
            let host_modules = get_host_modules()?.into_iter().collect::<HashSet<String>>();
            modules
                .par_iter()
                .filter(|(name, path, _deps)| {
                    (host_modules.contains(name) && is_module_needed(name, path))
                        || additional_modules.contains(name)
                })
                .map(|(name, _, _)| name.clone())
                .collect()
        }
    };

    let with_deps = expand_dependencies(&modules, selected_names);

    Ok(modules
        .iter()
        .filter(|(name, _, _)| with_deps.contains(name))
        .map(|(_, path, _)| kroot.join(path))
        .collect())
}

/// modules.dep lines list a module's own prerequisite modules (already-resolved transitively by
/// `depmod`) after the colon; walk that graph so a module pulled in for any reason (heuristic
/// match, `additional_modules`, or a host-loaded module) brings its dependencies along too,
/// instead of silently shipping a module that will fail to `init_module` at boot.
fn expand_dependencies(
    modules: &[(String, Utf8PathBuf, Vec<String>)],
    selected: HashSet<String>,
) -> HashSet<String> {
    let deps_by_name: std::collections::HashMap<&str, &Vec<String>> = modules
        .iter()
        .map(|(name, _, deps)| (name.as_str(), deps))
        .collect();

    let mut result = HashSet::new();
    let mut stack: Vec<String> = selected.into_iter().collect();
    while let Some(name) = stack.pop() {
        if !result.insert(name.clone()) {
            continue;
        }
        if let Some(deps) = deps_by_name.get(name.as_str()) {
            stack.extend(deps.iter().cloned());
        }
    }
    result
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

fn get_all_modules(kroot: &Utf8Path) -> Result<Vec<(String, Utf8PathBuf, Vec<String>)>> {
    BufReader::new(
        File::open(kroot.join("modules.dep")).with_context(|| "unable to open modules.dep")?,
    )
    .lines()
    .map_while(Result::ok)
    .map(|line| -> Result<(String, Utf8PathBuf, Vec<String>)> {
        let mut parts = line.splitn(2, ':');
        let module_path = Utf8Path::new(
            parts
                .next()
                .with_context(|| "unable to get module from modules.dep")?,
        );
        let deps = parts
            .next()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|dep| get_module_name(Utf8Path::new(dep)).ok())
            .collect();
        Ok((
            get_module_name(module_path)?,
            module_path.to_path_buf(),
            deps,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_kroot() -> Utf8PathBuf {
        Utf8PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test"))
    }

    #[test]
    fn get_all_modules_keeps_dependency_names() {
        let modules = get_all_modules(&test_kroot()).unwrap();
        let (_, _, deps) = modules
            .iter()
            .find(|(name, _, _)| name == "nvidia-uvm")
            .expect("nvidia-uvm not found");
        assert_eq!(deps, &vec!["nvidia".to_string()]);

        let (_, _, deps) = modules
            .iter()
            .find(|(name, _, _)| name == "qrtr-mhi")
            .expect("qrtr-mhi not found");
        let mut deps = deps.clone();
        deps.sort();
        assert_eq!(
            deps,
            vec!["mhi".to_string(), "ns".to_string(), "qrtr".to_string()]
        );
    }

    #[test]
    fn get_modules_transitively_includes_dependencies() {
        // nvidia-uvm doesn't match any is_module_needed() heuristic on its own, so it's only
        // included via additional_modules; its dependency "nvidia" must be pulled in too even
        // though nothing directly requested it, or init_module() for nvidia-uvm would fail at
        // boot with a missing dependency.
        let modules = get_modules(
            InitramfsType::General,
            &test_kroot(),
            vec!["nvidia-uvm".to_string()],
        )
        .unwrap();

        let names: HashSet<String> = modules
            .iter()
            .map(|path| get_module_name(path).unwrap())
            .collect();
        assert!(names.contains("nvidia-uvm"));
        assert!(
            names.contains("nvidia"),
            "dependency of an explicitly-requested module was dropped: {:?}",
            names
        );
    }
}
