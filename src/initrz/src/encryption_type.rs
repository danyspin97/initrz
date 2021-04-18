use std::convert::TryFrom;

use anyhow::{bail, Result};

pub enum EncryptionType {
    Luks,
}

impl TryFrom<&str> for EncryptionType {
    type Error = anyhow::Error;

    fn try_from(encryption: &str) -> Result<EncryptionType> {
        Ok(match encryption {
            "luks" => EncryptionType::Luks,
            _ => bail!("{} is not a supported encryption type", encryption),
        })
    }
}
