use anyhow::{bail, Context, Result};
use file_format::FileFormat;
use glob::{glob, Pattern};
use log::{debug, warn};
use nix::kmod::init_module;
use xz2::bufread::XzDecoder;
use zstd;

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub struct ModAlias {
    pattern: Pattern,
    module: String,
}

pub struct Module {
    pub filename: String,
    pub deps: Vec<String>,
}

pub struct ModuleLoader {
    modules: HashMap<String, Module>,
    aliases: Vec<ModAlias>,
    modules_loaded: RwLock<HashSet<String>>,
    kernel_root: PathBuf,
}

pub fn parse_module_dep(filename: &Path) -> Result<HashMap<String, Module>> {
    let file =
        File::open(filename).with_context(|| format!("unable to open filename {:?}", filename))?;
    let lines = BufReader::new(file).lines();
    Ok(lines
        .map_while(Result::ok)
        .map(|line| -> Result<(String, Module)> {
            let token_index = line
                .find(':')
                .with_context(|| format!("could not find ':' in line:\n{}", line))?;
            let module_filename = &line[0..token_index];
            let module = if let Ok(module) = get_module_name(module_filename) {
                module
            } else {
                bail!("{} is not a valid module name", module_filename);
            };
            let mut deps: Vec<String> = Vec::new();
            let rest_of_line = &line[token_index + 1..];
            if !rest_of_line.is_empty() {
                // iter.rest() returns " kernel/..." so skip the first space
                let split = rest_of_line[1..].split(' ');
                split
                    .filter_map(|dep| get_module_name(dep).ok())
                    .for_each(|dep| deps.push(dep));

                deps.reverse();
            }
            Ok((
                module,
                Module {
                    filename: module_filename.to_string(),
                    deps,
                },
            ))
        })
        .filter_map(|res| res.ok())
        .collect())
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

pub fn parse_module_alias(filename: &Path) -> Result<Vec<ModAlias>> {
    let file =
        File::open(filename).with_context(|| format!("unable to open file {:?}", filename))?;
    let lines = BufReader::new(file).lines();

    Ok(lines
        .map_while(Result::ok)
        .map(|line| -> Result<ModAlias> {
            let rest = line
                .strip_prefix("alias ")
                .with_context(|| format!("line does not start with 'alias ': {:?}", line))?;
            let mut split = rest.splitn(2, ' ');
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

        modules.reserve(glob(&kernel_root.join("*.ko.{xz,zst}").as_os_str().to_string_lossy())?.count());
        Ok(ModuleLoader {
            modules: parse_module_dep(&kernel_root.join("modules.dep"))?,
            aliases: parse_module_alias(&kernel_root.join("modules.alias"))?,
            modules_loaded: RwLock::new(modules),
            kernel_root,
        })
    }

    pub fn load_module(&self, module_name: &str) -> Result<bool> {
        if self.modules_loaded.read().unwrap().contains(module_name) {
            return Ok(true);
        }

        let module = match self.modules.get(module_name) {
            Some(m) => m,
            None => return Ok(false),
        };

        debug!("loading module {}", module_name);

        // Load dependencies first (they handle their own dedup)
        for dep in &module.deps {
            self.load_module(dep)?;
        }

        // Claim the slot under write lock; if another thread beat us here, skip loading
        {
            let mut modules_loaded = self.modules_loaded.write().unwrap();
            if modules_loaded.contains(module_name) {
                return Ok(true);
            }
            modules_loaded.insert(module_name.to_string());
        }

        // From here on, unclaim the slot on any early return so a failed load can be retried
        // instead of being silently treated as already loaded.
        match self.load_module_file(module_name, &module.filename) {
            Ok(loaded) => {
                if !loaded {
                    self.modules_loaded.write().unwrap().remove(module_name);
                }
                Ok(loaded)
            }
            Err(err) => {
                self.modules_loaded.write().unwrap().remove(module_name);
                Err(err)
            }
        }
    }

    fn load_module_file(&self, module_name: &str, module_filename: &str) -> Result<bool> {
        let filename = self.kernel_root.join(module_filename);
        let module_file =
            File::open(&filename).with_context(|| format!("unable to open {:?}", filename))?;

        let buf = match FileFormat::from_file(&filename)? {
            FileFormat::Zstandard => zstd::stream::decode_all(BufReader::new(module_file))?,
            FileFormat::Xz => {
                let mut buf = Vec::new();
                XzDecoder::new(BufReader::new(module_file)).read_to_end(&mut buf)?;
                buf
            }
            fmt => {
                warn!(
                    "unsupported format for module {}: {}",
                    filename.display(),
                    fmt
                );
                return Ok(false);
            }
        };

        init_module(&buf, &CString::new("")?)
            .with_context(|| format!("init_module failed when loading {}", module_name))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_module_dep_test() {
        let filename = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/test/module.dep"));
        let map = parse_module_dep(filename).unwrap();

        let mut expected_map = HashMap::new();

        let mhi_deps: Vec<String> = ["mhi", "ns", "qrtr"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected_map.insert(
            "qrtr-mhi".to_string(),
            Module {
                filename: String::from("kernel/net/qrtr/qrtr-mhi.ko.xz"),
                deps: mhi_deps,
            },
        );

        let nvidia_uvm_deps: Vec<String> = vec!["nvidia".to_string()];
        expected_map.insert(
            "nvidia-uvm".to_string(),
            Module {
                filename: String::from("kernel/drivers/video/nvidia-uvm.ko.xz"),
                deps: nvidia_uvm_deps,
            },
        );

        expected_map.insert(
            "nvidia".to_string(),
            Module {
                filename: String::from("kernel/drivers/video/nvidia.ko.xz"),
                deps: Vec::new(),
            },
        );

        assert_eq!(map.len(), 3);
        for (module_name, module) in map {
            let expected_module = expected_map.get(&module_name).expect("no module found");
            assert_eq!(module.filename, expected_module.filename);
            assert_eq!(module.deps, expected_module.deps);
        }
    }

    #[test]
    fn parse_module_alias_skips_malformed_lines_without_panicking() {
        let filename = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/test/modules.alias"));
        let aliases = parse_module_alias(filename).unwrap();

        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[0].module, "e1000e");
        assert_eq!(aliases[1].module, "usbmod");
    }
}
