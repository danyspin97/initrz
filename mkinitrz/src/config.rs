use std::{fs, path::Path};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub modules: Vec<String>,
}

impl Config {
    pub fn new(file: &Path) -> Result<Config> {
        if file.exists() {
            Ok(serde_yaml::from_slice(&fs::read(file)?)?)
        } else {
            Ok(Config {
                modules: Vec::new(),
            })
        }
    }
}
