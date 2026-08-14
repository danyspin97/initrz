use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use log::{info, warn};

/// Attempt to resume from a hibernation image on `devpath`.
///
/// This writes the device's major:minor to /sys/power/resume, which the
/// kernel handles synchronously: if a valid hibernation image is found there
/// it restores it and this call never effectively returns (the resumed
/// kernel takes over). If there is no valid image, or hibernation support is
/// unavailable, the write fails or is a no-op and boot continues normally -
/// so any error here is logged and treated as non-fatal.
pub fn try_resume(devpath: &str, offset: Option<u64>) -> Result<()> {
    let devname = Path::new(devpath)
        .file_name()
        .with_context(|| format!("unable to get device name from {:?}", devpath))?
        .to_str()
        .with_context(|| "device name is not valid UTF-8")?;

    let dev_id_path = Path::new("/sys/class/block").join(devname).join("dev");
    let dev_id = fs::read_to_string(&dev_id_path)
        .with_context(|| format!("unable to read {:?}", dev_id_path))?;
    let dev_id = dev_id.trim();

    if let Some(offset) = offset {
        if let Err(err) = fs::write("/sys/power/resume_offset", offset.to_string()) {
            warn!(
                "unable to set /sys/power/resume_offset to {}: {}",
                offset, err
            );
        }
    }

    info!("attempting resume from {} ({})", devpath, dev_id);
    if let Err(err) = fs::write("/sys/power/resume", dev_id) {
        warn!(
            "no hibernation image resumed from {}: {}; continuing normal boot",
            devpath, err
        );
    }

    Ok(())
}
