use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::{bail, Context, Result};
use log::warn;

pub struct Module {
    pub filename: String,
    pub deps: Vec<String>,
}

pub struct Modules {
    data: HashMap<String, Module>,
}

impl Modules {
    pub fn new(kernel_root: &Path) -> Result<Modules> {
        Ok(Modules {
            data: parse_module_dep(&kernel_root.join("modules.dep"))?,
        })
    }

    pub fn get(&self, module: &str) -> Option<&Module> {
        self.data.get(module)
    }
}

pub fn parse_module_dep(filename: &Path) -> Result<HashMap<String, Module>> {
    let file =
        File::open(filename).with_context(|| format!("unable to open filename {:?}", filename))?;
    let lines = BufReader::new(file).lines();
    Ok(lines
        .filter_map(|line: Result<String, _>| line.ok())
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
            if rest_of_line.len() != 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_module_dep_test() {
        let filename = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/test/module.dep"));
        let map = parse_module_dep(filename).unwrap();

        let mut expected_map = HashMap::new();

        let mut mhi_deps: Vec<String> = Vec::new();
        mhi_deps.push("mhi".to_string());
        mhi_deps.push("ns".to_string());
        mhi_deps.push("qrtr".to_string());
        expected_map.insert(
            "qrtr-mhi".to_string(),
            Module {
                filename: String::from("kernel/net/qrtr/qrtr-mhi.ko.xz"),
                deps: mhi_deps,
            },
        );

        let mut nvidia_uvm_deps: Vec<String> = Vec::new();
        nvidia_uvm_deps.push("nvidia".to_string());
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
}
