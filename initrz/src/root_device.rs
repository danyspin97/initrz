use std::convert::TryInto;

use anyhow::{Context, Result};

use crate::filesystem::Filesystem;
use crate::identifier::Identifier;

pub struct RootDevice {
    pub filesystem: Filesystem,
    pub identifier: Identifier,
    pub devpath: Option<String>,
}

pub fn get_root_from_cmdline(cmdline: &[String]) -> Result<RootDevice> {
    let auto_type = String::from("root.type=auto");

    let identifier = cmdline
        .iter()
        .filter(|arg| arg.starts_with("root="))
        .last()
        .with_context(|| "unable to find root device from command lines")?
        .strip_prefix("root=")
        .unwrap();

    Ok(RootDevice {
        identifier: if let Some(uuid) = identifier.strip_prefix("UUID=") {
            Identifier::Uuid(String::from(uuid))
        } else {
            Identifier::Path(String::from(identifier))
        },
        filesystem: cmdline
            .iter()
            .filter(|arg| arg.starts_with("root.type="))
            .last()
            .or(Some(&auto_type))
            .map(|root| root.strip_prefix("root.type="))
            .unwrap()
            .unwrap()
            .try_into()?,
        devpath: None,
    })
}
