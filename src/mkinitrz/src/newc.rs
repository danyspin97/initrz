// https://github.com/Farenjihn/elusive/blob/151b7e8080b75944327f949cbf2eab25490e5341/src/newc.rs
//! Newc cpio implementation
//!
//! This module implements the cpio newc format
//! that can be used with the Linux kernel to
//! load an initramfs.

use anyhow::Result;
use std::convert::TryInto;
use std::ffi::CString;
use std::fmt;
use std::fs::Metadata;
use std::io::Write;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Magic number for newc cpio files
const MAGIC: &[u8] = b"070701";
/// Magic bytes for cpio trailer entries
const TRAILER: &str = "TRAILER!!!";

/// Offset for inode number to avoid reserved inodes (arbitrary)
const INO_OFFSET: u64 = 1337;

/// Represents a cpio archive
#[derive(PartialEq, Debug)]
pub struct Archive {
    entries: Vec<Entry>,
}

impl Archive {
    /// Create a new archive from the provided entries
    pub fn new(entries: Vec<Entry>) -> Self {
        Archive { entries }
    }

    /// Serialize this entry into cpio newc format
    pub fn into_bytes(self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // iterate and lazily assign new inode number
        for (index, mut entry) in self.entries.into_iter().enumerate() {
            entry.ino = INO_OFFSET + index as u64;
            entry.write(&mut buf)?;
        }

        let trailer = EntryBuilder::trailer().build();
        trailer.write(&mut buf)?;

        Ok(buf)
    }
}

/// Represent the name of a cpio entry
#[derive(PartialEq, Default)]
pub struct EntryName {
    name: Vec<u8>,
}

impl EntryName {
    /// Get a null byte terminated vector for this entry name
    pub fn into_bytes_with_nul(self) -> Result<Vec<u8>> {
        let cstr = CString::new(self.name)?;
        Ok(cstr.into_bytes_with_nul())
    }
}

impl<T> From<T> for EntryName
where
    T: AsRef<Path>,
{
    fn from(path: T) -> Self {
        let path = path.as_ref();

        let stripped = if path.has_root() {
            path.strip_prefix("/").expect("path starts with /")
        } else {
            path
        };

        EntryName {
            name: stripped.as_os_str().as_bytes().to_vec(),
        }
    }
}

impl fmt::Debug for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryName")
            .field("name", &String::from_utf8_lossy(&self.name))
            .finish()
    }
}

/// Wrapper type for data
#[derive(PartialEq)]
pub struct EntryData {
    data: Vec<u8>,
}

impl EntryData {
    fn new(data: Vec<u8>) -> Self {
        EntryData { data }
    }
}

impl Deref for EntryData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl fmt::Debug for EntryData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EntryData(<data>)")
    }
}

/// Cpio newc entry
#[derive(PartialEq, Default, Debug)]
pub struct Entry {
    /// Name of the entry (path)
    name: EntryName,
    /// Inode of the entry
    ino: u64,
    /// Mode of the entry
    mode: u32,
    /// User id of the entry
    uid: u64,
    /// Group id of the entry
    gid: u64,
    /// Number of links to the entry
    nlink: u64,
    /// Modification time of the entry
    mtime: u64,
    /// Device major number of the entry
    dev_major: u64,
    /// Device minor number of the entry
    dev_minor: u64,
    /// Rdev major number of the entry
    rdev_major: u64,
    /// Rdev minor number of the entry
    rdev_minor: u64,
    /// Data is entry is a regular file or symlink
    data: Option<EntryData>,
}

impl Entry {
    /// Create an entry with the provided name
    fn new<T>(name: T) -> Self
    where
        T: Into<EntryName>,
    {
        Entry {
            name: name.into(),
            ..Entry::default()
        }
    }

    /// Create an entry with a name and data
    fn with_data<T>(name: T, data: Vec<u8>) -> Self
    where
        T: Into<EntryName>,
    {
        Entry {
            name: name.into(),
            data: Some(EntryData::new(data)),
            ..Entry::default()
        }
    }
}

impl Entry {
    /// Serialize the entry to the passed buffer
    pub fn write(self, buf: &mut Vec<u8>) -> Result<()> {
        let file_size = match &self.data {
            Some(data) => data.len(),
            None => 0,
        };

        // serialize the header for this entry
        let filename = self.name.into_bytes_with_nul()?;

        // magic + 8 * fields + filename + file
        buf.reserve(6 + (13 * 8) + filename.len() + file_size);
        buf.write_all(MAGIC)?;
        write!(buf, "{:08x}", self.ino)?;
        write!(buf, "{:08x}", self.mode)?;
        write!(buf, "{:08x}", self.uid)?; // uid is always 0 (root)
        write!(buf, "{:08x}", self.gid)?; // gid is always 0 (root)
        write!(buf, "{:08x}", self.nlink)?;
        write!(buf, "{:08x}", self.mtime)?;
        write!(buf, "{:08x}", file_size as usize)?;
        write!(buf, "{:08x}", self.dev_major)?; // dev_major is always 0
        write!(buf, "{:08x}", self.dev_minor)?; // dev_minor is always 0
        write!(buf, "{:08x}", self.rdev_major)?;
        write!(buf, "{:08x}", self.rdev_minor)?;
        write!(buf, "{:08x}", filename.len())?;
        write!(buf, "{:08x}", 0)?; // CRC, null bytes with our MAGIC
        buf.write_all(&filename)?;
        pad_buf(buf);

        if let Some(data) = &self.data {
            buf.write_all(data)?;
            pad_buf(buf);
        }

        Ok(())
    }
}

/// Builder pattern for a cpio entry
pub struct EntryBuilder {
    /// Entry being built
    entry: Entry,
}

impl EntryBuilder {
    /// Create an entry representing a directory
    pub fn directory<T>(name: T) -> Self
    where
        T: Into<EntryName>,
    {
        EntryBuilder {
            entry: Entry::new(name),
        }
    }

    /// Create an entry representing a regular file
    pub fn file<T>(name: T, data: Vec<u8>) -> Self
    where
        T: Into<EntryName>,
    {
        EntryBuilder {
            entry: Entry::with_data(name, data),
        }
    }

    /// Create an entry representing a special file
    pub fn special_file<T>(name: T) -> Self
    where
        T: Into<EntryName>,
    {
        EntryBuilder {
            entry: Entry::new(name),
        }
    }

    /// Create an entry representing a symlink
    pub fn symlink<T>(name: T, path: &Path) -> Self
    where
        T: Into<EntryName>,
    {
        let data = path.as_os_str().as_bytes().to_vec();
        EntryBuilder {
            entry: Entry::with_data(name, data),
        }
    }

    /// Create a trailer entry
    pub fn trailer() -> Self {
        EntryBuilder {
            entry: Entry::new(TRAILER),
        }
    }

    /// Add the provided metadata to the entry
    pub fn with_metadata(self, metadata: &Metadata) -> Self {
        let rdev = metadata.rdev();

        self.mode(metadata.mode())
            .mtime(
                metadata
                    .mtime()
                    .try_into()
                    .expect("timestamp does not fit in a u64"),
            )
            .rdev_major(major(rdev))
            .rdev_minor(minor(rdev))
    }

    /// Set the mode for the entry
    pub const fn mode(mut self, mode: u32) -> Self {
        self.entry.mode = mode;
        self
    }

    /// Set the modification time for the entry
    pub const fn mtime(mut self, mtime: u64) -> Self {
        self.entry.mtime = mtime;
        self
    }

    /// Set the major rdev number for the entry
    pub const fn rdev_major(mut self, rdev_major: u64) -> Self {
        self.entry.rdev_major = rdev_major;
        self
    }

    /// Set the minor rdev number for the entry
    pub const fn rdev_minor(mut self, rdev_minor: u64) -> Self {
        self.entry.rdev_minor = rdev_minor;
        self
    }

    /// Build the entry
    pub fn build(self) -> Entry {
        self.entry
    }
}

/// Pad the buffer so entries align according to cpio requirements
pub fn pad_buf(buf: &mut Vec<u8>) {
    let rem = buf.len() % 4;

    if rem != 0 {
        buf.resize(buf.len() + (4 - rem), 0);
    }
}

/// Shamelessly taken from the `nix` crate, thanks !
pub const fn major(dev: u64) -> u64 {
    ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0000_0fff)
}

/// Shamelessly taken from the `nix` crate, thanks !
pub const fn minor(dev: u64) -> u64 {
    ((dev >> 12) & 0xffff_ff00) | ((dev) & 0x0000_00ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() -> Result<()> {
        let entry = EntryBuilder::file("/testfile", b"datadatadata".to_vec()).build();

        let mut buf = Vec::new();
        entry.write(&mut buf)?;

        assert!(buf.len() > 0);

        Ok(())
    }

    #[test]
    fn test_serialize() -> Result<()> {
        let empty = Archive::new(Vec::new());
        let trailer = EntryBuilder::trailer().build();

        let mut buf = Vec::new();
        trailer.write(&mut buf)?;

        // an empty archive is just a trailer entry
        assert_eq!(empty.into_bytes()?, buf);

        Ok(())
    }
}
