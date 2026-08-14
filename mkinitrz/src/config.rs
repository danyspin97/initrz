use std::fs;

use anyhow::Result;
use camino::Utf8Path;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    /// Extra kernel modules to include by name (e.g. ["dm_crypt", "r8169"])
    #[serde(default)]
    pub modules: Vec<String>,
    /// Extra ELF binaries to include with their shared library dependencies
    #[serde(default)]
    pub binaries: Vec<String>,
    /// Extra plain files to include verbatim (source path = destination path)
    #[serde(default)]
    pub files: Vec<String>,
}

impl Config {
    pub fn new(file: &Utf8Path) -> Result<Config> {
        if file.exists() {
            Ok(serde_yaml::from_slice(&fs::read(file)?)?)
        } else {
            Ok(Config::default())
        }
    }
}
