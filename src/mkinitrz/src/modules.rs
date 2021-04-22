use std::{
    collections::HashMap,
    convert::TryFrom,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use dowser::Dowser;
use log::warn;
use rayon::prelude::*;

use crate::module::Module;

pub type Modules = Vec<Module>;

pub fn get_general_modules(kver: &str, additional_modules: Vec<String>) -> Result<Modules> {
    let modules: Vec<String> = get_modules_path(kver)?
        .iter_mut()
        .map(|(key, _)| key.clone())
        .collect();

    // let additional_modules: Vec<&str> = additional_modules
    //     .iter()
    //     .filter(|module| modules.contains(&module.as_str()).clone())
    //     .map(|s| s.as_str())
    //     .collect();
    // modules.extend(additional_modules);

    common_modules_gen(
        kver,
        &modules
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>()
            .as_slice(),
    )
}

pub fn get_host_modules(kver: &str, additional_modules: Vec<String>) -> Result<Modules> {
    let mut modules = Vec::new();
    let additional_modules: Vec<&str> = additional_modules
        .iter()
        .filter(|module| modules.contains(&module.as_str()).clone())
        .map(|s| s.as_str())
        .collect();
    modules.extend(additional_modules);

    common_modules_gen(kver, &modules)
}

fn common_modules_gen(kver: &str, modules: &[&str]) -> Result<Modules> {
    let modules_path = get_modules_path(kver)?;

    Ok(modules
        .par_iter()
        .map(|name| -> Result<Module> {
            let module_name = name.to_string();
            let path = modules_path
                .get(&module_name)
                .with_context(|| format!("unable to find module {}", name))?
                .clone();
            Ok(Module {
                name: module_name,
                path,
            })
        })
        .filter_map(|module| -> Option<Module> {
            if module.is_err() {
                warn!("{:?}", module);
            }
            module.ok()
        })
        .collect())
}

fn get_modules_path(kver: &str) -> Result<HashMap<String, PathBuf>> {
    Ok(Vec::<PathBuf>::try_from(
        Dowser::filtered(|p: &Path| {
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
        })
        .with_path(Path::new("/lib/modules").join(kver).join("kernel")),
    )?
    .iter()
    .map(|path| -> Result<(String, PathBuf)> {
        Ok((
            get_module_name(
                path.as_os_str()
                    .to_str()
                    .with_context(|| "unable to convert path to string")?,
            )?,
            path.clone(),
        ))
    })
    .collect::<Result<HashMap<String, PathBuf>>>()?)
}

fn get_module_name(filename: &str) -> Result<String> {
    Ok(Path::new(filename)
        .file_stem()
        .and_then(|module| std::path::Path::new(module).file_stem())
        .with_context(|| format!("failed to get module name of file {}", filename))?
        .to_str()
        .with_context(|| {
            format!(
                "failed to convert the module name in file {} from OsStr to Str",
                filename
            )
        })?
        .to_string())
}
