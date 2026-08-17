use std::fs;
use std::path::Path;

use anyhow::Result;
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
    // Every step below is best-effort per the doc comment above: a malformed devpath or a
    // /sys/class/block entry that isn't populated yet (e.g. a race during early boot) should
    // just skip resume, not abort the whole boot - so failures here are warned about and this
    // function returns Ok(()) instead of propagating with `?`.
    let Some(devname) = Path::new(devpath).file_name().and_then(|n| n.to_str()) else {
        warn!(
            "unable to get a valid device name from {:?}; skipping resume",
            devpath
        );
        return Ok(());
    };

    let dev_id_path = Path::new("/sys/class/block").join(devname).join("dev");
    let dev_id = match fs::read_to_string(&dev_id_path) {
        Ok(dev_id) => dev_id,
        Err(err) => {
            warn!("unable to read {:?}: {}; skipping resume", dev_id_path, err);
            return Ok(());
        }
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_resume_is_non_fatal_for_malformed_devpath() {
        // Neither of these devpaths reaches the /sys/power/* writes below (real system state we
        // must not touch from a test): "" has no file_name() at all, and a devname that doesn't
        // exist under /sys/class/block fails the dev_id read first. Both must return Ok(()),
        // matching the doc comment's "any error here is non-fatal" contract, not propagate.
        assert!(try_resume("", None).is_ok());
        assert!(try_resume("/dev/initrz-test-nonexistent-device", None).is_ok());
    }
}
