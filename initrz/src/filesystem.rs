use std::convert::TryFrom;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::utils::get_blkid_cache;

#[derive(Debug, PartialEq, Eq)]
pub enum Filesystem {
    Auto,
    Ext4,
    Btrfs,
    Xfs,
    F2fs,
    Vfat,
    Exfat,
}

impl TryFrom<&str> for Filesystem {
    type Error = anyhow::Error;

    fn try_from(filesystem: &str) -> Result<Self, Self::Error> {
        Ok(match filesystem {
            "auto" => Filesystem::Auto,
            "ext4" => Filesystem::Ext4,
            "btrfs" => Filesystem::Btrfs,
            "xfs" => Filesystem::Xfs,
            "f2fs" => Filesystem::F2fs,
            "vfat" => Filesystem::Vfat,
            "exfat" => Filesystem::Exfat,
            _ => bail!("{} is not a supported filesystem", filesystem),
        })
    }
}

impl std::convert::TryFrom<String> for Filesystem {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.as_str().try_into()
    }
}

impl Filesystem {
    pub fn get_filesystem_string(&self, path: &str) -> Result<String> {
        Ok(match self {
            Filesystem::Auto => get_blkid_cache()
                .get_tag_value("TYPE", &PathBuf::from(path))
                .with_context(|| format!("unable to get filesystem type for device {:?}", path))?,
            Filesystem::Ext4 => "ext4".to_string(),
            Filesystem::Btrfs => "btrfs".to_string(),
            Filesystem::Xfs => "xfs".to_string(),
            Filesystem::F2fs => "f2fs".to_string(),
            Filesystem::Vfat => "vfat".to_string(),
            Filesystem::Exfat => "exfat".to_string(),
        })
    }
}
