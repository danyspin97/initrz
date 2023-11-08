// https://github.com/Farenjihn/elusive/blob/151b7e8080b75944327f949cbf2eab25490e5341/src/depend.rs

use anyhow::{bail, Result};
use log::error;
use object::{
    elf::{FileHeader32, FileHeader64, DT_NEEDED, DT_STRSZ, DT_STRTAB, PT_DYNAMIC},
    read::{
        elf::{Dyn, FileHeader, ProgramHeader},
        FileKind,
    },
    Endianness, StringTable,
};
use std::convert::TryInto;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub fn resolve(path: &Path) -> Result<Vec<PathBuf>> {
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
            error!("Failed to parse binary");
            bail!("only elf files are supported");
        }
    }?;

    let mut resolved = Vec::new();

    for lib in needed {
        walk_linkmap(&lib, &mut resolved)?;
    }

    Ok(resolved)
}

fn elf_needed<T>(elf: &T, data: &[u8]) -> Result<Vec<OsString>>
where
    T: FileHeader<Endian = Endianness>,
{
    let endian = elf.endian()?;
    let headers = elf.program_headers(endian, data)?;

    let mut strtab = 0;
    let mut strsz = 0;

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
                let offset = offset.try_into()?;
                let name = dynstr.get(offset).expect("offset exists in string table");
                let path = OsStr::from_bytes(name).to_os_string();

                needed.push(path);
            }

            break;
        }
    }

    Ok(needed)
}

fn walk_linkmap(lib: &OsStr, resolved: &mut Vec<PathBuf>) -> Result<()> {
    let name = CString::new(lib.as_bytes())?;
    let mut linkmap = MaybeUninit::<*mut link_map>::uninit();

    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY) };
    if handle.is_null() {
        let error = unsafe {
            CStr::from_ptr(libc::dlerror())
                .to_str()
                .expect("error should be valid utf8")
        };

        error!("Failed to open handle to dynamic dependency for {:?}", lib);
        bail!("dlopen failed: {}", error);
    }

    let ret = unsafe {
        libc::dlinfo(
            handle,
            libc::RTLD_DI_LINKMAP,
            linkmap.as_mut_ptr() as *mut libc::c_void,
        )
    };

    if ret < 0 {
        error!("Failed to get path to dynamic dependency for {:?}", lib);
        bail!("dlinfo failed");
    }

    let mut names = Vec::new();
    unsafe {
        let mut linkmap = linkmap.assume_init();

        // walk back to the beginning of the link map
        while !(*linkmap).l_prev.is_null() {
            linkmap = (*linkmap).l_prev as *mut link_map;
        }

        // skip first entry in linkmap since its name is empty
        // next entry is also skipped since it is the vDSO
        linkmap = (*linkmap).l_next as *mut link_map;

        // walk through the link map and add entries
        while !(*linkmap).l_next.is_null() {
            linkmap = (*linkmap).l_next as *mut link_map;
            names.push(CStr::from_ptr((*linkmap).l_name));
        }
    };

    for name in names {
        let path = PathBuf::from(OsStr::from_bytes(name.to_bytes()));
        resolved.push(path);
    }

    let ret = unsafe { libc::dlclose(handle) };
    if ret < 0 {
        error!("Failed to close handle to dynamic dependency for {:?}", lib);
        bail!("dlclose failed");
    }

    Ok(())
}

/// C struct used in `dlinfo` with `RTLD_DI_LINKMAP`
#[repr(C)]
#[allow(non_camel_case_types)]
struct link_map {
    l_addr: u64,
    l_name: *mut libc::c_char,
    l_ld: *mut libc::c_void,
    l_next: *mut libc::c_void,
    l_prev: *mut libc::c_void,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver() -> Result<()> {
        let ls = PathBuf::from("/bin/ls");

        if ls.exists() {
            let dependencies = resolve(&ls)?;
            let mut found_libc = false;

            for lib in dependencies {
                if lib
                    .file_name()
                    .expect("library path should have filename")
                    .to_str()
                    .expect("filename should be valid utf8")
                    .starts_with("libc")
                {
                    found_libc = true;
                    break;
                }
            }

            if !found_libc {
                bail!("resolver did not list libc in dependencies")
            }
        }

        Ok(())
    }
}
