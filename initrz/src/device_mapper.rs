use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

use anyhow::{bail, Context, Result};
use nix::ioctl_readwrite;

// From <linux/dm-ioctl.h>. This is the same UAPI that dmsetup/libdevmapper use; encoding the
// ioctl request number against the fixed dm_ioctl header (not the larger buffer that follows
// it) matches how those tools build the request, since the kernel's DM ioctl handler reads the
// real length from data_size/data_start inside the struct rather than from the ioctl number.
const DM_IOCTL: u8 = 0xfd;
const DM_NAME_LEN: usize = 128;
const DM_UUID_LEN: usize = 129;
const DM_MAX_TYPE_NAME: usize = 16;
const DM_INTERFACE_VERSION: [u32; 3] = [4, 0, 0];
// Room for the target_spec array + parameter strings appended after the header. Generous for
// even a few dozen striped segments; create_and_activate() errors out instead of overflowing it.
const DM_TABLE_BUF_LEN: usize = 16384;

#[repr(C)]
struct DmIoctlHeader {
    version: [u32; 3],
    data_size: u32,
    data_start: u32,
    target_count: u32,
    open_count: i32,
    flags: u32,
    event_nr: u32,
    padding: u32,
    dev: u64,
    name: [u8; DM_NAME_LEN],
    uuid: [u8; DM_UUID_LEN],
    data: [u8; 7],
}

#[repr(C)]
struct DmIoctlBuffer {
    header: DmIoctlHeader,
    table: [u8; DM_TABLE_BUF_LEN],
}

#[repr(C)]
struct DmTargetSpecHeader {
    sector_start: u64,
    length: u64,
    status: i32,
    next: u32,
    target_type: [u8; DM_MAX_TYPE_NAME],
}

ioctl_readwrite!(dm_dev_create, DM_IOCTL, 0x03, DmIoctlHeader);
ioctl_readwrite!(dm_dev_remove, DM_IOCTL, 0x04, DmIoctlHeader);
ioctl_readwrite!(dm_dev_suspend, DM_IOCTL, 0x06, DmIoctlHeader);
ioctl_readwrite!(dm_table_load, DM_IOCTL, 0x09, DmIoctlHeader);

/// One row of a device-mapper table: `<start_sector> <length_sectors> <target_type> <params>`,
/// the same shape as a line of `dmsetup create` input.
#[derive(Debug)]
pub struct DmTarget {
    pub start_sector: u64,
    pub length_sectors: u64,
    pub target_type: String,
    pub params: String,
}

fn new_header(name: &str) -> Result<DmIoctlHeader> {
    if name.len() >= DM_NAME_LEN {
        bail!("device-mapper device name {:?} too long", name);
    }
    let mut header = DmIoctlHeader {
        version: DM_INTERFACE_VERSION,
        data_size: std::mem::size_of::<DmIoctlHeader>() as u32,
        data_start: std::mem::size_of::<DmIoctlHeader>() as u32,
        target_count: 0,
        open_count: 0,
        flags: 0,
        event_nr: 0,
        padding: 0,
        dev: 0,
        name: [0u8; DM_NAME_LEN],
        uuid: [0u8; DM_UUID_LEN],
        data: [0u8; 7],
    };
    header.name[..name.len()].copy_from_slice(name.as_bytes());
    Ok(header)
}

/// Encode `targets` as a sequence of raw `dm_target_spec` entries (spec header + NUL-terminated,
/// 8-byte-aligned parameter string, one entry per target) into `buf`, starting at offset 0.
/// Returns the total number of bytes written. Pure host-side encoding — no fd, no ioctl — kept
/// separate from create_and_activate() so the wire layout (offsets, padding, `next` pointers)
/// can be unit-tested without a real device-mapper device.
fn encode_targets(targets: &[DmTarget], buf: &mut [u8]) -> Result<usize> {
    let spec_size = std::mem::size_of::<DmTargetSpecHeader>();
    let mut offset = 0usize;
    for target in targets {
        if target.target_type.len() >= DM_MAX_TYPE_NAME {
            bail!(
                "device-mapper target type {:?} too long",
                target.target_type
            );
        }

        let params_bytes = target.params.as_bytes();
        // NUL-terminate the param string and pad to an 8-byte boundary, so the next
        // dm_target_spec in the buffer stays aligned as the kernel expects.
        let unpadded = params_bytes.len() + 1;
        let padded = (unpadded + 7) & !7;
        let entry_size = spec_size + padded;

        if offset + entry_size > buf.len() {
            bail!(
                "device-mapper table is too large ({} targets, {}-byte buffer)",
                targets.len(),
                buf.len()
            );
        }

        let mut spec = DmTargetSpecHeader {
            sector_start: target.start_sector,
            length: target.length_sectors,
            status: 0,
            next: entry_size as u32,
            target_type: [0u8; DM_MAX_TYPE_NAME],
        };
        spec.target_type[..target.target_type.len()].copy_from_slice(target.target_type.as_bytes());

        let spec_bytes = unsafe {
            std::slice::from_raw_parts(&spec as *const DmTargetSpecHeader as *const u8, spec_size)
        };
        buf[offset..offset + spec_size].copy_from_slice(spec_bytes);
        buf[offset + spec_size..offset + spec_size + params_bytes.len()]
            .copy_from_slice(params_bytes);
        // Trailing NUL + padding bytes are already zeroed by the caller.

        offset += entry_size;
    }
    Ok(offset)
}

fn open_control() -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")
        .with_context(|| {
            "unable to open /dev/mapper/control (is the dm_mod module loaded/built in?)"
        })
}

/// Create a device-mapper device called `name`, load `targets` as its table, and activate it —
/// equivalent to `dmsetup create <name>` fed the same table. On success the kernel emits a
/// uevent for the resulting dm-N device, which uevent_listener.rs already handles to create
/// /dev/mapper/<name>.
pub fn create_and_activate(name: &str, targets: &[DmTarget]) -> Result<()> {
    if targets.is_empty() {
        bail!(
            "refusing to activate device-mapper device {:?} with no targets",
            name
        );
    }

    let ctrl = open_control()?;

    let mut header = new_header(name)?;
    unsafe { dm_dev_create(ctrl.as_raw_fd(), &mut header) }
        .with_context(|| format!("DM_DEV_CREATE failed for {:?}", name))?;

    // From here on, the device-mapper node exists in the kernel (and a uevent for it has
    // already fired). If anything below fails, remove it instead of returning early and leaving
    // a stray, tableless /dev/mapper/<name> behind - otherwise a retry after fixing the error
    // would fail DM_DEV_CREATE with "device already exists".
    let activate = (|| -> Result<()> {
        let mut buf = DmIoctlBuffer {
            header: new_header(name)?,
            table: [0u8; DM_TABLE_BUF_LEN],
        };
        let offset = encode_targets(targets, &mut buf.table)
            .with_context(|| format!("encoding device-mapper table for {:?}", name))?;
        buf.header.target_count = targets.len() as u32;
        buf.header.data_size = (std::mem::size_of::<DmIoctlHeader>() + offset) as u32;

        unsafe { dm_table_load(ctrl.as_raw_fd(), &mut buf.header) }
            .with_context(|| format!("DM_TABLE_LOAD failed for {:?}", name))?;

        // DM_DEV_SUSPEND without the suspend flag set resumes the device instead, which is what
        // makes the just-loaded (inactive) table live — the same create+load+resume sequence
        // `dmsetup create` performs.
        let mut header = new_header(name)?;
        unsafe { dm_dev_suspend(ctrl.as_raw_fd(), &mut header) }
            .with_context(|| format!("DM_DEV_SUSPEND (resume) failed for {:?}", name))?;

        Ok(())
    })();

    if let Err(err) = activate {
        if let Ok(mut remove_header) = new_header(name) {
            if let Err(remove_err) = unsafe { dm_dev_remove(ctrl.as_raw_fd(), &mut remove_header) }
            {
                log::warn!(
                    "unable to remove partially-activated device-mapper device {:?} after error: {:?}",
                    name,
                    remove_err
                );
            }
        }
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One decoded `dm_target_spec` entry, mirroring what a real DM_TABLE_LOAD reader (e.g. the
    /// kernel, or `dmsetup table`) would see: sector_start/length/target_type from the spec
    /// header, plus the NUL-terminated param string that follows it.
    #[derive(Debug, PartialEq)]
    struct DecodedTarget {
        sector_start: u64,
        length: u64,
        target_type: String,
        params: String,
    }

    /// Walk the `next`-linked chain of `dm_target_spec` entries out of an encode_targets() buffer,
    /// the same way the kernel does. Used only by tests, to check the encoder's output byte-for-
    /// byte rather than just trusting it round-trips through itself.
    fn decode_targets(buf: &[u8], count: usize) -> Vec<DecodedTarget> {
        let spec_size = std::mem::size_of::<DmTargetSpecHeader>();
        let mut decoded = Vec::new();
        let mut offset = 0usize;
        for _ in 0..count {
            let spec = unsafe { &*(buf[offset..].as_ptr() as *const DmTargetSpecHeader) };
            let type_end = spec
                .target_type
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(spec.target_type.len());
            let target_type = std::str::from_utf8(&spec.target_type[..type_end])
                .unwrap()
                .to_string();

            let params_start = offset + spec_size;
            let params_end = buf[params_start..]
                .iter()
                .position(|&b| b == 0)
                .map(|i| params_start + i)
                .expect("params not NUL-terminated");
            let params = std::str::from_utf8(&buf[params_start..params_end])
                .unwrap()
                .to_string();

            decoded.push(DecodedTarget {
                sector_start: spec.sector_start,
                length: spec.length,
                target_type,
                params,
            });

            assert_ne!(spec.next, 0, "next pointer should never be 0 mid-chain");
            offset += spec.next as usize;
        }
        decoded
    }

    fn target(start: u64, len: u64, ty: &str, params: &str) -> DmTarget {
        DmTarget {
            start_sector: start,
            length_sectors: len,
            target_type: ty.to_string(),
            params: params.to_string(),
        }
    }

    #[test]
    fn encodes_a_single_linear_target() {
        let targets = vec![target(0, 2048, "linear", "8:0 2048")];
        let mut buf = [0u8; DM_TABLE_BUF_LEN];
        let n = encode_targets(&targets, &mut buf).unwrap();

        let decoded = decode_targets(&buf[..n], targets.len());
        assert_eq!(
            decoded,
            vec![DecodedTarget {
                sector_start: 0,
                length: 2048,
                target_type: "linear".to_string(),
                params: "8:0 2048".to_string(),
            }]
        );
    }

    #[test]
    fn encodes_multiple_targets_with_correct_offsets_and_alignment() {
        let targets = vec![
            target(0, 100, "linear", "8:0 0"),
            // A longer param string than the first, to exercise a different padding amount.
            target(100, 200, "striped", "2 128 8:0 100 8:1 100"),
            target(300, 50, "linear", "8:0 400"),
        ];
        let mut buf = [0u8; DM_TABLE_BUF_LEN];
        let n = encode_targets(&targets, &mut buf).unwrap();

        let spec_size = std::mem::size_of::<DmTargetSpecHeader>();
        // Every entry (spec header + NUL-terminated params, padded) must land on an 8-byte
        // boundary, and the whole encoded region must be a multiple of 8 bytes long.
        assert_eq!(n % 8, 0);
        assert!(
            spec_size.is_multiple_of(8),
            "spec header itself must be 8-aligned"
        );

        let decoded = decode_targets(&buf[..n], targets.len());
        assert_eq!(
            decoded,
            vec![
                DecodedTarget {
                    sector_start: 0,
                    length: 100,
                    target_type: "linear".to_string(),
                    params: "8:0 0".to_string(),
                },
                DecodedTarget {
                    sector_start: 100,
                    length: 200,
                    target_type: "striped".to_string(),
                    params: "2 128 8:0 100 8:1 100".to_string(),
                },
                DecodedTarget {
                    sector_start: 300,
                    length: 50,
                    target_type: "linear".to_string(),
                    params: "8:0 400".to_string(),
                },
            ]
        );
    }

    #[test]
    fn rejects_a_target_type_that_does_not_fit() {
        let targets = vec![target(0, 100, "this-target-type-is-way-too-long", "params")];
        let mut buf = [0u8; DM_TABLE_BUF_LEN];
        assert!(encode_targets(&targets, &mut buf).is_err());
    }

    #[test]
    fn rejects_a_table_that_overflows_the_buffer() {
        let targets: Vec<_> = (0..4).map(|_| target(0, 100, "linear", "8:0 0")).collect();
        let mut small_buf = [0u8; 32]; // not even room for one entry
        assert!(encode_targets(&targets, &mut small_buf).is_err());
    }

    #[test]
    fn dm_ioctl_header_matches_the_known_kernel_uapi_size() {
        // sizeof(struct dm_ioctl) from <linux/dm-ioctl.h> has been 312 bytes for the entire
        // lifetime of the interface (interface version 4.x). If this ever changes, data_start/
        // data_size and the encoded ioctl request numbers (which bake in size_of::<DmIoctlHeader>())
        // would silently go wrong.
        assert_eq!(std::mem::size_of::<DmIoctlHeader>(), 312);
    }

    #[test]
    fn dm_target_spec_header_matches_the_known_kernel_uapi_size() {
        assert_eq!(std::mem::size_of::<DmTargetSpecHeader>(), 40);
    }
}
