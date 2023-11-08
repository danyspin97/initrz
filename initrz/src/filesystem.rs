use std::{convert::TryFrom, path::PathBuf};

use anyhow::{bail, Context, Result};

use crate::utils::get_blkid_cache;

#[derive(PartialEq, Eq)]
pub enum Filesystem {
    Auto,
    Ext4,
}

impl TryFrom<&str> for Filesystem {
    type Error = anyhow::Error;

    fn try_from(filesystem: &str) -> Result<Self, Self::Error> {
        Ok(match filesystem {
            "ext4" => Filesystem::Ext4,
            "auto" => Filesystem::Auto,
            _ => bail!("{} is not a supported filesystem", filesystem),
        })
    }
}

impl Filesystem {
    pub fn get_filesystem_string(&self, path: &str) -> Result<String> {
        Ok(match self {
            Filesystem::Ext4 => String::from("ext4"),
            Filesystem::Auto => get_blkid_cache()
                .get_tag_value("TYPE", &PathBuf::from(path))
                .with_context(|| format!("unable to get filesystem type for device {:?}", path))?,
        })
    }
}
