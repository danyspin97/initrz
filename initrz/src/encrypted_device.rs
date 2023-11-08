use std::convert::TryInto;

use anyhow::{Context, Result};

use crate::encryption_type::EncryptionType;
use crate::identifier::Identifier;
use crate::unlock_type::UnlockType;

pub struct EncryptedDevice {
    pub name: String,
    pub identifier: Identifier,
    pub encryption_type: EncryptionType,
    pub unlock: UnlockType,
}

impl EncryptedDevice {
    pub fn from_line(line: String) -> Result<EncryptedDevice> {
        let mut split = line.split_whitespace();
        Ok(EncryptedDevice {
            name: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .to_string(),
            identifier: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .into(),
            encryption_type: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .try_into()?,
            unlock: split
                .next()
                .with_context(|| format!("unable to find name in line:\n{}", line))?
                .into(),
        })
    }
}
