use std::{fmt, path::Path};

use anyhow::{bail, Result};
use either::Right;

use crate::utils::get_blkid_cache;

const UUID_TAG: &str = "UUID";

#[derive(PartialEq, Eq)]
pub enum Identifier {
    Path(String),
    Uuid(String),
}

impl From<&str> for Identifier {
    fn from(identifier: &str) -> Identifier {
        if identifier.starts_with("UUID=") {
            Identifier::Uuid(identifier[5..].to_string())
        } else {
            Identifier::Path(identifier.to_string())
        }
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self {
            Identifier::Path(path) => write!(f, "{:?}", path),
            Identifier::Uuid(uuid) => write!(f, "{}", uuid),
        }
    }
}

impl Identifier {
    pub fn get_path(&self) -> Result<String> {
        Ok(match self {
            Identifier::Uuid(uuid) => {
                String::from(get_blkid_cache().get_devname(Right((&UUID_TAG, &uuid)))?)
            }
            Identifier::Path(path) => {
                if Path::new(path).exists() {
                    bail!("unable to find device in path {:?}", path);
                }
                path.clone()
            }
        })
    }
}
