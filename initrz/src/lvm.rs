use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, Context, Result};
use libblkid_rs::BlkidCache;
use log::{info, warn};

use crate::device_mapper::{self, DmTarget};
use crate::utils::get_blkid_cache;

const SECTOR_SIZE: u64 = 512;
// LVM2 always writes the PV label to sector 1 in practice (the spec allows sectors 0-3, but
// every LVM2 version in the wild only ever uses sector 1), so we don't bother scanning.
const LABEL_SECTOR: u64 = 1;

/// Segment types this implementation knows how to turn into a device-mapper table. Anything
/// else (mirror, raid*, thin-pool, thin, cache, snapshot, ...) is rejected with a clear error
/// instead of being silently misactivated — same reasoning as deferring native mdadm RAID
/// assembly: those layouts need real correctness work this doesn't attempt yet. LVM's "striped"
/// segment type covers both plain linear segments (stripe_count == 1) and true striping
/// (stripe_count > 1); which DM target that becomes is decided separately, since they need
/// different params shapes.
fn is_supported_segment_type(lvm_segment_type: &str) -> bool {
    lvm_segment_type == "striped"
}

struct PvLabel {
    /// Raw 32-character PV UUID as stored in the binary header, without dashes.
    uuid: String,
    metadata_descriptor: (u64, u64),
}

/// Parse the LABELONE sector and PV header of a physical volume. Both live in sector 1,
/// regardless of the PV's own metadata area layout — see the "Physical Volume Label" section of
/// the LVM2 on-disk format spec.
fn read_pv_label<R: Read + Seek>(file: &mut R) -> Result<PvLabel> {
    let mut sector = [0u8; SECTOR_SIZE as usize];
    file.seek(SeekFrom::Start(LABEL_SECTOR * SECTOR_SIZE))
        .context("seeking to PV label sector")?;
    file.read_exact(&mut sector)
        .context("reading PV label sector")?;

    // Layout: "LABELONE"[0..8], sector_number u64[8..16], checksum u32[16..20],
    // data_offset u32[20..24], "LVM2 001"[24..32].
    if &sector[0..8] != b"LABELONE" {
        bail!("not an LVM2 physical volume (missing LABELONE magic)");
    }
    if &sector[24..32] != b"LVM2 001" {
        bail!("unsupported LVM label type (expected \"LVM2 001\")");
    }
    let data_offset = u32::from_le_bytes(sector[20..24].try_into().unwrap()) as usize;
    if data_offset >= sector.len() || sector.len() - data_offset < 40 {
        bail!("PV label data_offset {} out of range", data_offset);
    }

    let header = &sector[data_offset..];
    let uuid = std::str::from_utf8(&header[0..32])
        .context("PV UUID is not valid UTF-8")?
        .to_string();

    // After pv_ident (32 bytes) and pv_size (u64, 8 bytes) come two null-terminated arrays of
    // 16-byte (offset, size) descriptors: data areas first, then metadata areas. We only need
    // the first metadata area.
    let mut pos = 32 + 8;
    // Skip the data_descriptors array (we don't need it: DM targets address PVs by their
    // resolved device major:minor plus pe_start from the text metadata, not by this table).
    loop {
        if pos + 16 > header.len() {
            bail!("PV header data_descriptors array runs past the end of the label sector");
        }
        let offset = u64::from_le_bytes(header[pos..pos + 8].try_into().unwrap());
        let size = u64::from_le_bytes(header[pos + 8..pos + 16].try_into().unwrap());
        pos += 16;
        if offset == 0 && size == 0 {
            break;
        }
    }

    if pos + 16 > header.len() {
        bail!("PV header metadata_descriptors array runs past the end of the label sector");
    }
    let metadata_offset = u64::from_le_bytes(header[pos..pos + 8].try_into().unwrap());
    let metadata_size = u64::from_le_bytes(header[pos + 8..pos + 16].try_into().unwrap());
    if metadata_offset == 0 && metadata_size == 0 {
        bail!("PV header has no metadata area descriptor");
    }

    Ok(PvLabel {
        uuid,
        metadata_descriptor: (metadata_offset, metadata_size),
    })
}

/// Read the (single) location descriptor's worth of text metadata out of the metadata area
/// described by `metadata_descriptor` (offset, size — both absolute, in bytes).
fn read_vg_metadata_text<R: Read + Seek>(
    file: &mut R,
    metadata_descriptor: (u64, u64),
) -> Result<String> {
    let (mda_offset, _mda_size) = metadata_descriptor;

    let mut header = [0u8; SECTOR_SIZE as usize];
    file.seek(SeekFrom::Start(mda_offset))
        .context("seeking to metadata area header")?;
    file.read_exact(&mut header)
        .context("reading metadata area header")?;

    if &header[4..20] != b" LVM2 x[5A%r0N*>" {
        bail!("unrecognised metadata area header magic");
    }

    // First location descriptor: 8 bytes data_area_offset (relative to mda_offset), 8 bytes
    // data_area_size, 4 bytes checksum, 4 bytes flags.
    let locdesc_start = 4 + 16 + 4 + 8 + 8; // checksum, magic, version, mda_offset, mda_size
    let data_area_offset =
        u64::from_le_bytes(header[locdesc_start..locdesc_start + 8].try_into().unwrap());
    let data_area_size = u64::from_le_bytes(
        header[locdesc_start + 8..locdesc_start + 16]
            .try_into()
            .unwrap(),
    );
    if data_area_offset == 0 && data_area_size == 0 {
        bail!("metadata area has no location descriptors");
    }
    // VG text metadata is a handful of KiB in practice; LVM2 itself defaults the whole metadata
    // area to 1 MiB. Cap well above that (16 MiB) so a corrupted/torn-write size field can't make
    // us request a multi-exabyte allocation and abort the process (we run as PID 1) before
    // read_exact() ever gets a chance to fail normally on a short file.
    const MAX_METADATA_TEXT_SIZE: u64 = 16 * 1024 * 1024;
    if data_area_size > MAX_METADATA_TEXT_SIZE {
        bail!(
            "metadata area data_area_size {} exceeds sanity limit of {} bytes",
            data_area_size,
            MAX_METADATA_TEXT_SIZE
        );
    }

    let mut text = vec![0u8; data_area_size as usize];
    file.seek(SeekFrom::Start(mda_offset + data_area_offset))
        .context("seeking to metadata text")?;
    file.read_exact(&mut text)
        .context("reading metadata text")?;
    // The text metadata is a NUL-terminated (and NUL-padded) string within its allotted area.
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    text.truncate(end);

    String::from_utf8(text).context("VG metadata text is not valid UTF-8")
}

#[derive(Debug, Clone)]
pub struct PvDesc {
    /// PV UUID as written in the text metadata (dashed form).
    pub id: String,
    pub pe_start: u64,
}

#[derive(Debug, Clone)]
pub struct SegmentDesc {
    pub start_extent: u64,
    pub extent_count: u64,
    pub segment_type: String,
    pub stripe_count: u64,
    /// (pv_name, pe_start_in_extents) per stripe, in stripe order.
    pub stripes: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
pub struct LvDesc {
    pub segments: Vec<SegmentDesc>,
}

#[derive(Debug, Clone)]
pub struct VgMetadata {
    pub name: String,
    /// In sectors, matching the on-disk unit (`extent_size` in the text metadata).
    pub extent_size: u64,
    pub physical_volumes: BTreeMap<String, PvDesc>,
    pub logical_volumes: BTreeMap<String, LvDesc>,
}

mod metadata_text {
    use anyhow::{bail, Context, Result};

    #[derive(Debug, Clone)]
    pub enum Value {
        String(String),
        Number(i64),
        List(Vec<Value>),
    }

    impl Value {
        pub fn as_str(&self) -> Result<&str> {
            match self {
                Value::String(s) => Ok(s),
                _ => bail!("expected a string value, got {:?}", self),
            }
        }

        pub fn as_u64(&self) -> Result<u64> {
            match self {
                Value::Number(n) if *n >= 0 => Ok(*n as u64),
                _ => bail!("expected a non-negative number, got {:?}", self),
            }
        }

        pub fn as_list(&self) -> Result<&[Value]> {
            match self {
                Value::List(items) => Ok(items),
                _ => bail!("expected a list value, got {:?}", self),
            }
        }
    }

    #[derive(Debug, Clone)]
    pub enum Element {
        Value(Value),
        Section(Vec<(String, Element)>),
    }

    impl Element {
        pub fn as_section(&self) -> Result<&[(String, Element)]> {
            match self {
                Element::Section(fields) => Ok(fields),
                _ => bail!("expected a section, got a value"),
            }
        }

        pub fn as_value(&self) -> Result<&Value> {
            match self {
                Element::Value(v) => Ok(v),
                _ => bail!("expected a value, got a section"),
            }
        }
    }

    pub trait SectionExt {
        fn field(&self, name: &str) -> Result<&Element>;
    }

    impl SectionExt for [(String, Element)] {
        fn field(&self, name: &str) -> Result<&Element> {
            self.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v)
                .with_context(|| format!("missing field {:?}", name))
        }
    }

    struct Parser<'a> {
        input: &'a [u8],
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn skip_whitespace_and_comments(&mut self) {
            loop {
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_whitespace() {
                    self.pos += 1;
                }
                if self.pos < self.input.len() && self.input[self.pos] == b'#' {
                    while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    continue;
                }
                break;
            }
        }

        fn peek(&self) -> Option<u8> {
            self.input.get(self.pos).copied()
        }

        fn parse_ident(&mut self) -> Result<String> {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_whitespace() || matches!(c, b'{' | b'}' | b'=' | b'[' | b']') {
                    break;
                }
                self.pos += 1;
            }
            if self.pos == start {
                bail!("expected an identifier at byte {}", start);
            }
            Ok(std::str::from_utf8(&self.input[start..self.pos])?.to_string())
        }

        fn parse_string(&mut self) -> Result<String> {
            if self.peek() != Some(b'"') {
                bail!("expected '\"' at byte {}", self.pos);
            }
            self.pos += 1;
            let start = self.pos;
            while self.peek() != Some(b'"') {
                if self.pos >= self.input.len() {
                    bail!("unterminated string starting at byte {}", start);
                }
                self.pos += 1;
            }
            let s = std::str::from_utf8(&self.input[start..self.pos])?.to_string();
            self.pos += 1; // closing quote
            Ok(s)
        }

        fn parse_number(&mut self) -> Result<i64> {
            let start = self.pos;
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == start {
                bail!("expected a number at byte {}", start);
            }
            Ok(std::str::from_utf8(&self.input[start..self.pos])?.parse()?)
        }

        fn parse_value(&mut self) -> Result<Value> {
            self.skip_whitespace_and_comments();
            match self.peek() {
                Some(b'"') => Ok(Value::String(self.parse_string()?)),
                Some(b'[') => {
                    self.pos += 1;
                    let mut items = Vec::new();
                    loop {
                        self.skip_whitespace_and_comments();
                        if self.peek() == Some(b']') {
                            self.pos += 1;
                            break;
                        }
                        items.push(self.parse_value()?);
                        self.skip_whitespace_and_comments();
                        if self.peek() == Some(b',') {
                            self.pos += 1;
                        }
                    }
                    Ok(Value::List(items))
                }
                Some(c) if c == b'-' || c.is_ascii_digit() => {
                    Ok(Value::Number(self.parse_number()?))
                }
                other => bail!("unexpected byte {:?} while parsing a value", other),
            }
        }

        fn parse_elements(&mut self) -> Result<Vec<(String, Element)>> {
            let mut elements = Vec::new();
            loop {
                self.skip_whitespace_and_comments();
                match self.peek() {
                    None | Some(b'}') => break,
                    _ => {}
                }
                let name = self.parse_ident()?;
                self.skip_whitespace_and_comments();
                match self.peek() {
                    Some(b'=') => {
                        self.pos += 1;
                        let value = self.parse_value()?;
                        elements.push((name, Element::Value(value)));
                    }
                    Some(b'{') => {
                        self.pos += 1;
                        let contents = self.parse_elements()?;
                        self.skip_whitespace_and_comments();
                        if self.peek() != Some(b'}') {
                            bail!("expected '}}' to close section {:?}", name);
                        }
                        self.pos += 1;
                        elements.push((name, Element::Section(contents)));
                    }
                    other => bail!("expected '=' or '{{' after {:?}, got {:?}", name, other),
                }
            }
            Ok(elements)
        }
    }

    pub fn parse(input: &str) -> Result<Vec<(String, Element)>> {
        let mut parser = Parser {
            input: input.as_bytes(),
            pos: 0,
        };
        let elements = parser.parse_elements()?;
        parser.skip_whitespace_and_comments();
        if parser.pos != parser.input.len() {
            bail!("trailing garbage in VG metadata at byte {}", parser.pos);
        }
        Ok(elements)
    }
}

use metadata_text::{Element, SectionExt};

fn parse_segment(name: &str, section: &[(String, Element)]) -> Result<SegmentDesc> {
    let start_extent = section.field("start_extent")?.as_value()?.as_u64()?;
    let extent_count = section.field("extent_count")?.as_value()?.as_u64()?;
    let segment_type = section.field("type")?.as_value()?.as_str()?.to_string();
    let stripe_count = section
        .iter()
        .find(|(k, _)| k == "stripe_count")
        .map(|(_, v)| v.as_value().and_then(|v| v.as_u64()))
        .transpose()?
        .unwrap_or(1);

    let mut stripes = Vec::new();
    if let Some((_, stripes_elem)) = section.iter().find(|(k, _)| k == "stripes") {
        let items = stripes_elem.as_value()?.as_list()?;
        if items.len() % 2 != 0 {
            bail!("segment {:?} has an odd-length stripes list", name);
        }
        for pair in items.chunks_exact(2) {
            let pv_name = pair[0].as_str()?.to_string();
            let pe_start = pair[1].as_u64()?;
            stripes.push((pv_name, pe_start));
        }
    }

    Ok(SegmentDesc {
        start_extent,
        extent_count,
        segment_type,
        stripe_count,
        stripes,
    })
}

type NumberedSegment<'a> = (u64, &'a String, &'a [(String, Element)]);

fn parse_lv(section: &[(String, Element)]) -> Result<LvDesc> {
    let mut numbered_segments: Vec<NumberedSegment> = Vec::new();
    for (key, value) in section {
        if let Some(n) = key.strip_prefix("segment") {
            if let Ok(n) = n.parse::<u64>() {
                numbered_segments.push((n, key, value.as_section()?));
            }
        }
    }
    numbered_segments.sort_by_key(|(n, ..)| *n);

    let segments = numbered_segments
        .into_iter()
        .map(|(_, name, section)| parse_segment(name, section))
        .collect::<Result<Vec<_>>>()?;

    if segments.is_empty() {
        bail!("logical volume has no segments");
    }

    Ok(LvDesc { segments })
}

fn parse_pv(section: &[(String, Element)]) -> Result<PvDesc> {
    Ok(PvDesc {
        id: section.field("id")?.as_value()?.as_str()?.to_string(),
        pe_start: section.field("pe_start")?.as_value()?.as_u64()?,
    })
}

fn parse_vg_metadata(text: &str) -> Result<VgMetadata> {
    let top = metadata_text::parse(text)?;
    let (vg_name, vg_section) = top
        .iter()
        .find(|(_, v)| matches!(v, Element::Section(_)))
        .context("VG metadata has no top-level section")?;
    let vg_section = vg_section.as_section()?;

    let extent_size = vg_section.field("extent_size")?.as_value()?.as_u64()?;

    let mut physical_volumes = BTreeMap::new();
    for (name, elem) in vg_section.field("physical_volumes")?.as_section()? {
        physical_volumes.insert(name.clone(), parse_pv(elem.as_section()?)?);
    }

    let mut logical_volumes = BTreeMap::new();
    for (name, elem) in vg_section.field("logical_volumes")?.as_section()? {
        logical_volumes.insert(name.clone(), parse_lv(elem.as_section()?)?);
    }

    Ok(VgMetadata {
        name: vg_name.clone(),
        extent_size,
        physical_volumes,
        logical_volumes,
    })
}

/// Resolve a PV named in VG metadata to an actual device path via blkid, matching UUIDs with
/// dashes stripped from both sides — the text metadata always writes PV ids dashed, and blkid's
/// LVM2_member prober reports the dashed form too (confirmed against a real probe), but
/// stripping both sides is form-agnostic insurance either way.
///
/// Takes an already-probed `cache` (see `activate_volume_group`) rather than probing fresh per
/// call: each PV's own uevent independently triggers device_handler.rs's LVM2_member check, so
/// resolving every PV of a multi-PV VG needs a cache that's been probed since the last one of
/// them appeared - probing once up front for the whole VG activation covers that, instead of
/// each PV paying its own full probe_all_new() pass.
fn resolve_pv_device(cache: &BlkidCache, pv_id: &str) -> Result<String> {
    let wanted = pv_id.replace('-', "");
    for device in cache.iter() {
        let devname = device.devname()?;
        let matched = device
            .tag_iter()
            .any(|(tag, value)| tag == "UUID" && value.replace('-', "") == wanted);
        if matched {
            return Ok(devname.to_str().unwrap().to_string());
        }
    }
    bail!(
        "unable to find a device for PV {:?} (UUID {})",
        pv_id,
        pv_id
    )
}

fn major_minor(devpath: &str) -> Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta =
        std::fs::metadata(devpath).with_context(|| format!("unable to stat {:?}", devpath))?;
    let rdev = meta.rdev();
    // nix::sys::stat::major()/minor() decode st_rdev the same way glibc's gnu_dev_major()/
    // gnu_dev_minor() (bits/sysmacros.h) do - use the crate's version instead of hand-rolling the
    // same bit math (which uevent_listener.rs independently re-implements the inverse of).
    let major = nix::sys::stat::major(rdev) as u32;
    let minor = nix::sys::stat::minor(rdev) as u32;
    Ok((major, minor))
}

/// LVM escapes literal '-' in VG/LV names by doubling it, then joins the two with a single '-',
/// matching the convention real LVM uses for /dev/mapper/<vg>-<lv> node names.
fn dm_name(vg: &str, lv: &str) -> String {
    format!("{}-{}", vg.replace('-', "--"), lv.replace('-', "--"))
}

/// Parse the VG metadata found on the PV at `pv_path` and natively activate every logical
/// volume in that VG via device-mapper — the from-scratch equivalent of `vgchange -ay`. Logical
/// volumes with a segment type we don't support (anything but linear/striped) are logged and
/// skipped rather than aborting the whole VG.
pub fn activate_volume_group(pv_path: &str) -> Result<()> {
    let mut file = File::open(pv_path).with_context(|| format!("unable to open {:?}", pv_path))?;
    let label = read_pv_label(&mut file)?;
    let text = read_vg_metadata_text(&mut file, label.metadata_descriptor)?;
    let vg = parse_vg_metadata(&text)?;
    drop(file);

    // Sanity check: the metadata area we just parsed should describe a VG that actually
    // contains the PV we read it from. If it doesn't, something's corrupt (or we misparsed the
    // binary header) and every offset we're about to compute is suspect.
    if !vg
        .physical_volumes
        .values()
        .any(|pv| pv.id.replace('-', "") == label.uuid)
    {
        bail!(
            "VG metadata on {:?} doesn't list this PV (uuid {}) as a member",
            pv_path,
            label.uuid
        );
    }

    info!(
        "activating volume group {:?} ({} logical volume(s))",
        vg.name,
        vg.logical_volumes.len()
    );

    // Probed once up front and reused for every PV below, instead of each resolve_pv_device()
    // call opening its own handle and re-probing every block device from scratch - for an
    // N-PV volume group that turned VG activation into N full probe passes instead of one.
    let mut pv_cache = get_blkid_cache()?;
    pv_cache.probe_all_new()?;

    let mut pv_devices: BTreeMap<&str, (u32, u32, u64)> = BTreeMap::new();
    for (pv_name, pv) in &vg.physical_volumes {
        let devpath = resolve_pv_device(&pv_cache, &pv.id)
            .with_context(|| format!("resolving PV {:?} of VG {:?}", pv_name, vg.name))?;
        let (major, minor) = major_minor(&devpath)?;
        pv_devices.insert(pv_name, (major, minor, pv.pe_start));
    }

    for (lv_name, lv) in &vg.logical_volumes {
        let Some(targets) = build_dm_targets(&vg.name, &pv_devices, vg.extent_size, lv_name, lv)?
        else {
            continue;
        };
        if targets.is_empty() {
            continue;
        }

        let name = dm_name(&vg.name, lv_name);
        device_mapper::create_and_activate(&name, &targets)
            .with_context(|| format!("activating logical volume {}/{}", vg.name, lv_name))?;
    }

    Ok(())
}

/// Turn one logical volume's segments into device-mapper targets. Pure function of already-parsed
/// metadata and resolved PV device numbers (no I/O), so on-disk-geometry edge cases — overflow,
/// unsupported segment types, stripe-count mismatches — can be unit tested directly instead of
/// only through a real device-mapper activation.
///
/// Returns `Ok(None)` for a logical volume with an unsupported segment type (logged and skipped,
/// same as before: it shouldn't abort activation of the rest of the VG).
fn build_dm_targets(
    vg_name: &str,
    pv_devices: &BTreeMap<&str, (u32, u32, u64)>,
    extent_size: u64,
    lv_name: &str,
    lv: &LvDesc,
) -> Result<Option<Vec<DmTarget>>> {
    let mut targets = Vec::new();
    for segment in &lv.segments {
        if !is_supported_segment_type(&segment.segment_type) {
            warn!(
                "logical volume {}/{}: unsupported segment type {:?}, skipping activation",
                vg_name, lv_name, segment.segment_type
            );
            return Ok(None);
        };
        if segment.stripe_count as usize != segment.stripes.len() {
            bail!(
                "logical volume {}/{}: stripe_count {} doesn't match {} stripe(s)",
                vg_name,
                lv_name,
                segment.stripe_count,
                segment.stripes.len()
            );
        }

        let start_sector = segment
            .start_extent
            .checked_mul(extent_size)
            .with_context(|| {
                format!(
                    "logical volume {}/{}: start_extent * extent_size overflows u64",
                    vg_name, lv_name
                )
            })?;
        let length_sectors = segment
            .extent_count
            .checked_mul(extent_size)
            .with_context(|| {
                format!(
                    "logical volume {}/{}: extent_count * extent_size overflows u64",
                    vg_name, lv_name
                )
            })?;

        // A single-stripe segment is a plain "linear" DM target (<dev> <offset>). DM's
        // "striped" target is a different params shape (<#stripes> <chunk_size> <dev>
        // <offset> ...) that only applies once there's more than one stripe to interleave.
        let (target_type, params) = if segment.stripe_count == 1 {
            let (pv_name, pe_offset) = &segment.stripes[0];
            let &(major, minor, pe_start) = pv_devices
                .get(pv_name.as_str())
                .with_context(|| format!("segment references unknown PV {:?}", pv_name))?;
            let offset_sectors = pe_offset
                .checked_mul(extent_size)
                .and_then(|v| v.checked_add(pe_start))
                .with_context(|| {
                    format!(
                        "logical volume {}/{}: pe_start + pe_offset * extent_size overflows u64",
                        vg_name, lv_name
                    )
                })?;
            ("linear", format!("{}:{} {}", major, minor, offset_sectors))
        } else {
            // dm "striped" params: <#stripes> <chunk_size_sectors> <dev1> <off1> ...
            // LVM's default stripe unit is 64 KiB; without stripe_size recorded on the
            // segment (the text metadata field we don't currently read) we can't recover a
            // custom stripe size, so multi-stripe LVs beyond the default aren't supported
            // yet. Bail loudly instead of guessing at boot-critical geometry.
            bail!(
                "logical volume {}/{}: multi-stripe segments are not supported yet",
                vg_name,
                lv_name
            );
        };

        targets.push(DmTarget {
            start_sector,
            length_sectors,
            target_type: target_type.to_string(),
            params,
        });
    }

    Ok(Some(targets))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linear_single_pv_vg() {
        let text = r#"
# Generated by LVM2

vg0 {
	id = "AbCdEfGhIjKlMnOpQrStUvWxYz012345"
	seqno = 3
	format = "lvm2"
	status = ["RESIZEABLE", "READ", "WRITE"]
	flags = []
	extent_size = 8192
	max_lv = 0
	max_pv = 0
	metadata_copies = 0

	physical_volumes {
		pv0 {
			id = "abcdef01-2345-6789-abcd-ef0123456789"
			device = "/dev/sda2"
			status = ["ALLOCATABLE"]
			flags = []
			dev_size = 20971520
			pe_start = 2048
			pe_count = 2559
		}
	}

	logical_volumes {
		root {
			id = "fedcba98-7654-3210-fedc-ba9876543210"
			status = ["READ", "WRITE", "VISIBLE"]
			flags = []
			creation_time = 1234567890
			creation_host = "build"
			segment_count = 1

			segment1 {
				start_extent = 0
				extent_count = 2559
				type = "striped"
				stripe_count = 1

				stripes = [
					"pv0", 0
				]
			}
		}
	}
}
"#;
        let vg = parse_vg_metadata(text).unwrap();
        assert_eq!(vg.name, "vg0");
        assert_eq!(vg.extent_size, 8192);
        assert_eq!(vg.physical_volumes.len(), 1);
        assert_eq!(vg.physical_volumes["pv0"].pe_start, 2048);
        assert_eq!(vg.logical_volumes.len(), 1);
        let root = &vg.logical_volumes["root"];
        assert_eq!(root.segments.len(), 1);
        assert_eq!(root.segments[0].segment_type, "striped");
        assert_eq!(root.segments[0].stripe_count, 1);
        assert_eq!(root.segments[0].stripes, vec![("pv0".to_string(), 0)]);
    }

    #[test]
    fn parses_multi_segment_lv_in_extent_order_regardless_of_text_order() {
        let text = r#"
vg0 {
	id = "id"
	extent_size = 8192
	physical_volumes {
		pv0 {
			id = "id0"
			device = "/dev/sda2"
			pe_start = 2048
			pe_count = 100
		}
	}
	logical_volumes {
		data {
			id = "id1"
			segment_count = 2

			segment2 {
				start_extent = 50
				extent_count = 50
				type = "striped"
				stripe_count = 1
				stripes = ["pv0", 50]
			}
			segment1 {
				start_extent = 0
				extent_count = 50
				type = "striped"
				stripe_count = 1
				stripes = ["pv0", 0]
			}
		}
	}
}
"#;
        let vg = parse_vg_metadata(text).unwrap();
        let data = &vg.logical_volumes["data"];
        assert_eq!(data.segments[0].start_extent, 0);
        assert_eq!(data.segments[1].start_extent, 50);
    }

    #[test]
    fn rejects_unsupported_segment_types_without_erroring_the_whole_vg() {
        assert!(is_supported_segment_type("striped"));
        assert!(!is_supported_segment_type("mirror"));
        assert!(!is_supported_segment_type("raid1"));
        assert!(!is_supported_segment_type("thin"));
        assert!(!is_supported_segment_type("snapshot"));
    }

    #[test]
    fn dm_name_escapes_literal_hyphens() {
        assert_eq!(dm_name("vg0", "root"), "vg0-root");
        assert_eq!(dm_name("my-vg", "my-lv"), "my--vg-my--lv");
    }

    #[test]
    fn rejects_metadata_with_trailing_garbage() {
        assert!(metadata_text::parse("vg0 { id = \"x\" } garbage").is_err());
    }

    fn linear_lv(start_extent: u64, extent_count: u64, pe_offset: u64) -> LvDesc {
        LvDesc {
            segments: vec![SegmentDesc {
                start_extent,
                extent_count,
                segment_type: "striped".to_string(),
                stripe_count: 1,
                stripes: vec![("pv0".to_string(), pe_offset)],
            }],
        }
    }

    #[test]
    fn build_dm_targets_computes_sector_offsets() {
        let mut pv_devices = BTreeMap::new();
        pv_devices.insert("pv0", (8u32, 2u32, 2048u64));
        let lv = linear_lv(0, 100, 0);

        let targets = build_dm_targets("vg0", &pv_devices, 8192, "root", &lv)
            .unwrap()
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].start_sector, 0);
        assert_eq!(targets[0].length_sectors, 100 * 8192);
        assert_eq!(targets[0].params, "8:2 2048");
    }

    #[test]
    fn build_dm_targets_rejects_start_extent_overflow() {
        let mut pv_devices = BTreeMap::new();
        pv_devices.insert("pv0", (8u32, 2u32, 2048u64));
        let lv = linear_lv(u64::MAX, 1, 0);

        let err = build_dm_targets("vg0", &pv_devices, 8192, "root", &lv).unwrap_err();
        assert!(err
            .to_string()
            .contains("start_extent * extent_size overflows"));
    }

    #[test]
    fn build_dm_targets_rejects_extent_count_overflow() {
        let mut pv_devices = BTreeMap::new();
        pv_devices.insert("pv0", (8u32, 2u32, 2048u64));
        let lv = linear_lv(0, u64::MAX, 0);

        let err = build_dm_targets("vg0", &pv_devices, 8192, "root", &lv).unwrap_err();
        assert!(err
            .to_string()
            .contains("extent_count * extent_size overflows"));
    }

    #[test]
    fn build_dm_targets_rejects_pe_offset_overflow() {
        let mut pv_devices = BTreeMap::new();
        pv_devices.insert("pv0", (8u32, 2u32, 2048u64));
        let lv = linear_lv(0, 1, u64::MAX);

        let err = build_dm_targets("vg0", &pv_devices, 8192, "root", &lv).unwrap_err();
        assert!(err
            .to_string()
            .contains("pe_start + pe_offset * extent_size overflows"));
    }

    #[test]
    fn build_dm_targets_skips_unsupported_segment_type_without_erroring() {
        let mut pv_devices = BTreeMap::new();
        pv_devices.insert("pv0", (8u32, 2u32, 2048u64));
        let lv = LvDesc {
            segments: vec![SegmentDesc {
                start_extent: 0,
                extent_count: 10,
                segment_type: "mirror".to_string(),
                stripe_count: 1,
                stripes: vec![("pv0".to_string(), 0)],
            }],
        };

        assert!(build_dm_targets("vg0", &pv_devices, 8192, "root", &lv)
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_vg_metadata_text_rejects_absurd_data_area_size() {
        // Build a synthetic metadata-area header with a data_area_size field far beyond any real
        // VG's text metadata, and confirm it errors instead of attempting a huge allocation.
        let mut header = [0u8; SECTOR_SIZE as usize];
        header[4..20].copy_from_slice(b" LVM2 x[5A%r0N*>");
        let locdesc_start = 4 + 16 + 4 + 8 + 8;
        header[locdesc_start..locdesc_start + 8].copy_from_slice(&512u64.to_le_bytes());
        header[locdesc_start + 8..locdesc_start + 16].copy_from_slice(&u64::MAX.to_le_bytes());

        let mut cursor = std::io::Cursor::new(header.to_vec());
        let err = read_vg_metadata_text(&mut cursor, (0, 0)).unwrap_err();
        assert!(err.to_string().contains("exceeds sanity limit"));
    }

    /// Byte-for-byte construction of a minimal PV image (label sector + PV header + metadata
    /// area header + text metadata), matching the on-disk layout documented at
    /// https://github.com/libyal/libvslvm/blob/main/documentation/Logical%20Volume%20Manager%20(LVM)%20format.asciidoc.
    /// This is what actually caught the label-sector field-offset bug during development —
    /// the per-field unit tests above only exercise the text parser, never the binary offsets.
    fn build_test_pv_image(pv_uuid_dashless: &str, vg_text: &str) -> Vec<u8> {
        assert_eq!(pv_uuid_dashless.len(), 32);

        const MDA_OFFSET: u64 = 4096;
        const MDA_TEXT_REL_OFFSET: u64 = 512; // relative to MDA_OFFSET
        const MDA_TEXT_AREA_SIZE: u64 = 4096;

        let mut image = vec![0u8; (MDA_OFFSET + MDA_TEXT_REL_OFFSET + MDA_TEXT_AREA_SIZE) as usize];

        // --- Label sector (sector 1) ---
        let label = &mut image[512..1024];
        label[0..8].copy_from_slice(b"LABELONE");
        // sector_number[8..16], checksum[16..20]: left zeroed, unused by our reader.
        label[20..24].copy_from_slice(&32u32.to_le_bytes()); // data_offset
        label[24..32].copy_from_slice(b"LVM2 001");

        // --- PV header, starting at data_offset (32) within the same sector ---
        let pv_header = &mut label[32..];
        pv_header[0..32].copy_from_slice(pv_uuid_dashless.as_bytes());
        // pv_size[32..40]: unused by our reader.
        // One data_descriptor entry (offset=1MiB, size=0 meaning "rest of device"), then the
        // 16-byte zero terminator.
        pv_header[40..48].copy_from_slice(&(1024 * 1024u64).to_le_bytes());
        pv_header[48..56].copy_from_slice(&0u64.to_le_bytes());
        // [56..72] left zeroed: data_descriptors terminator.
        // One metadata_descriptor entry (offset, size), then implicitly the zero terminator
        // (left zeroed, we never read past the first entry).
        pv_header[72..80].copy_from_slice(&MDA_OFFSET.to_le_bytes());
        pv_header[80..88].copy_from_slice(&8192u64.to_le_bytes());

        // --- Metadata area header, at MDA_OFFSET ---
        let mda_header_start = MDA_OFFSET as usize;
        let mda_header = &mut image[mda_header_start..mda_header_start + 512];
        // checksum[0..4] left zeroed.
        mda_header[4..20].copy_from_slice(b" LVM2 x[5A%r0N*>");
        // version
        mda_header[20..24].copy_from_slice(&1u32.to_le_bytes());
        // metadata_area_offset[24..32], metadata_area_size[32..40]: unused by our reader.
        // First location descriptor at byte 40.
        mda_header[40..48].copy_from_slice(&MDA_TEXT_REL_OFFSET.to_le_bytes());
        mda_header[48..56].copy_from_slice(&(vg_text.len() as u64).to_le_bytes());

        // --- Metadata text ---
        let text_start = (MDA_OFFSET + MDA_TEXT_REL_OFFSET) as usize;
        image[text_start..text_start + vg_text.len()].copy_from_slice(vg_text.as_bytes());
        // Rest of the metadata text area is already zero (NUL padding).

        image
    }

    #[test]
    fn reads_pv_label_and_metadata_text_from_a_synthetic_image() {
        let vg_text = r#"
vg0 {
	id = "id"
	extent_size = 8192
	physical_volumes {
		pv0 {
			id = "61626364-6566-6768-696a-6b6c6d6e6f70"
			device = "/dev/sda2"
			pe_start = 2048
			pe_count = 100
		}
	}
	logical_volumes {
		root {
			id = "id1"
			segment_count = 1
			segment1 {
				start_extent = 0
				extent_count = 100
				type = "striped"
				stripe_count = 1
				stripes = ["pv0", 0]
			}
		}
	}
}
"#;
        let pv_uuid = "abcdefghijklmnopqrstuvwxyz012345";
        let image = build_test_pv_image(pv_uuid, vg_text);
        let mut cursor = std::io::Cursor::new(image);

        let label = read_pv_label(&mut cursor).unwrap();
        assert_eq!(label.uuid, pv_uuid);

        let text = read_vg_metadata_text(&mut cursor, label.metadata_descriptor).unwrap();
        let vg = parse_vg_metadata(&text).unwrap();
        assert_eq!(vg.name, "vg0");
        assert_eq!(vg.logical_volumes["root"].segments[0].extent_count, 100);
    }
}
