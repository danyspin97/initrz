use std::{
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::Result;
use xz2::bufread::XzDecoder;

#[derive(Debug)]
pub struct Module {
    pub name: String,
    pub path: PathBuf,
}

impl Module {
    pub fn new(name: String, path: &Path) -> Module {
        Module {
            name,
            path: path.into(),
        }
    }

    pub fn into_bytes(&self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        XzDecoder::new(BufReader::new(File::open(&self.path)?)).read_to_end(&mut data)?;
        Ok(data)
    }
}
