use anyhow::{bail, Context, Result};
use dowser::Dowser;
use glob::{glob, Pattern};
use log::{debug, warn};
use nix::kmod::{init_module, ModuleInitFlags};
use xz2::bufread::XzDecoder;

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::mem::drop;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use common::Modules;

pub struct ModAlias {
    pattern: Pattern,
    module: String,
}

pub struct ModuleLoader {
    modules: Modules,
    aliases: Vec<ModAlias>,
    modules_loaded: RwLock<HashSet<String>>,
    kernel_root: PathBuf,
}

pub fn parse_module_alias(filename: &Path) -> Result<Vec<ModAlias>> {
    let file =
        File::open(filename).with_context(|| format!("unable to open file {:?}", filename))?;
    let lines = BufReader::new(file).lines();

    Ok(lines
        .filter_map(|line: Result<String, _>| line.ok())
        .map(|line| -> Result<ModAlias> {
            let mut split = line[6..].splitn(2, ' ');
            Ok(ModAlias {
                pattern: Pattern::new(split.next().with_context(|| "no pattern found")?)?,
                module: split
                    .next()
                    .with_context(|| "no modalias found")?
                    .to_string(),
            })
        })
        .filter_map(|res| {
            if res.is_err() {
                warn!("unable to parse modalias line");
            }
            res.ok()
        })
        .collect())
}

impl ModuleLoader {
    pub fn init(kernel_version: &str) -> Result<ModuleLoader> {
        let kernel_root = Path::new("/lib/modules").join(kernel_version);
        let mut modules = HashSet::new();

        modules.reserve(glob(&kernel_root.join("*.ko.xz").as_os_str().to_string_lossy())?.count());
        Ok(ModuleLoader {
            modules: Modules::new(&kernel_root)?,
            aliases: parse_module_alias(&kernel_root.join("modules.alias"))?,
            modules_loaded: RwLock::new(modules),
            kernel_root,
        })
    }

    pub fn load_module(&self, module_name: &str) -> Result<bool> {
        let modules_loaded = self.modules_loaded.read().unwrap();
        if !modules_loaded.contains(module_name) {
            drop(modules_loaded);
            debug!("loading module {}", module_name);
            let module = self.modules.get(module_name);
            if module.is_none() {
                return Ok(false);
            }
            let module = module.unwrap();
            // Some modules could be builtin, do not block
            module.deps.iter().try_for_each(|dep| -> Result<()> {
                self.load_module(&dep)?;
                Ok(())
            })?;
            let mut modules_loaded = self.modules_loaded.write().unwrap();
            if !modules_loaded.contains(module_name) {
                modules_loaded.insert(String::from(module_name));
            }
            // unlock so that other modules can be loaded in parallel
            drop(modules_loaded);
            let filename = self.kernel_root.join(&module.filename);
            let module_file =
                File::open(&filename).with_context(|| format!("unable to find {:?}", filename))?;
            let mut buf = Vec::new();
            XzDecoder::new(BufReader::new(module_file)).read_to_end(&mut buf)?;

            init_module(&buf, &CString::new("")?).with_context(|| {
                format!("finit_module call failed when loading {}", module_name)
            })?;
        }

        Ok(true)
    }

    pub fn load_modalias(&self, modalias: &str) -> Result<()> {
        let modalias = &self.aliases.iter().find(|m| m.pattern.matches(modalias));
        if let Some(modalias) = modalias {
            self.load_module(&modalias.module)?;
        }

        Ok(())
    }
}
