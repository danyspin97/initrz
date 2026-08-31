// https://github.com/Farenjihn/elusive/blob/151b7e8080b75944327f949cbf2eab25490e5341/src/depend.rs

use anyhow::{bail, Context, Result};
use camino::Utf8Path;
use log::error;
use object::{
    elf::{FileHeader32, FileHeader64, DT_NEEDED, DT_STRSZ, DT_STRTAB, PT_DYNAMIC},
    read::{
        elf::{Dyn, FileHeader, ProgramHeader},
        FileKind,
    },
    Endianness, StringTable,
};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;

/// Returns the ELF class (32- vs 64-bit) of the file at `path`, without doing the full
/// DT_NEEDED walk that `resolve()` does. Used to reject a same-named library found on a
/// different architecture's search path (e.g. a multiarch `/usr/lib/i386-linux-gnu` picked up
/// alongside `/usr/lib/x86_64-linux-gnu` from `ld.so.conf.d`).
pub fn elf_kind(path: &Utf8Path) -> Result<FileKind> {
    let data = fs::read(path)?;
    FileKind::parse(&*data).with_context(|| format!("unable to parse ELF header of {}", path))
}

pub fn resolve(path: &Utf8Path) -> Result<Vec<String>> {
    let data = fs::read(path)?;
    let kind = FileKind::parse(&*data)?;

    let needed = match kind {
        FileKind::Elf32 => {
            let elf = FileHeader32::<Endianness>::parse(&*data)?;
            elf_needed(elf, &data)
        }
        FileKind::Elf64 => {
            let elf = FileHeader64::<Endianness>::parse(&*data)?;
            elf_needed(elf, &data)
        }
        _ => {
            error!("Failed to parse binary at {}", path);
            bail!("only ELF files are supported");
        }
    }?;

    needed
        .into_iter()
        .map(|s| {
            s.to_str()
                .map(str::to_string)
                .with_context(|| format!("library name {:?} is not valid UTF-8", s))
        })
        .collect()
}

fn elf_needed<T>(elf: &T, data: &[u8]) -> Result<Vec<std::ffi::OsString>>
where
    T: FileHeader<Endian = Endianness>,
{
    let endian = elf.endian()?;
    let headers = elf.program_headers(endian, data)?;

    let mut strtab = 0u64;
    let mut strsz = 0u64;
    let mut offsets = Vec::new();

    for header in headers {
        if header.p_type(endian) == PT_DYNAMIC {
            if let Some(dynamic) = header.dynamic(endian, data)? {
                for entry in dynamic {
                    let d_tag = entry.d_tag(endian).into();
                    if d_tag == DT_STRTAB as u64 {
                        strtab = entry.d_val(endian).into();
                    } else if d_tag == DT_STRSZ as u64 {
                        strsz = entry.d_val(endian).into();
                    } else if d_tag == DT_NEEDED as u64 {
                        offsets.push(entry.d_val(endian).into());
                    }
                }
            }
        }
    }

    let mut needed = Vec::new();

    for header in headers {
        if let Ok(Some(data)) = header.data_range(endian, data, strtab, strsz) {
            let dynstr = StringTable::new(data, 0, strsz);
            for offset in offsets {
                let name = dynstr.get(offset as u32).ok().with_context(|| {
                    format!("DT_NEEDED offset {} out of range in string table", offset)
                })?;
                needed.push(OsStr::from_bytes(name).to_os_string());
            }
            break;
        }
    }

    Ok(needed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver() -> Result<()> {
        let ls = Utf8Path::new("/bin/ls");
        if ls.exists() {
            let deps = resolve(ls)?;
            assert!(
                deps.iter().any(|lib| lib.starts_with("libc")),
                "expected libc in dependencies of /bin/ls, got: {:?}",
                deps
            );
        }
        Ok(())
    }
}
