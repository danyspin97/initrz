use std::{
    collections::HashSet,
    convert::TryInto,
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use log::debug;

use crate::config::Config;
use crate::depend;
use crate::initramfs_modules;
use crate::initramfs_type::InitramfsType;
use crate::newc::{self, Archive, Entry, EntryBuilder};

const ROOT_DIRECTORIES: [&str; 9] = [
    "/dev",
    "/etc",
    "/new_root",
    "/proc",
    "/run",
    "/sys",
    "/usr",
    "/usr/bin",
    "/usr/lib",
];

const ROOT_SYMLINKS: [(&str, &str); 6] = [
    ("/bin", "usr/bin"),
    ("/lib", "usr/lib"),
    ("/lib64", "lib"),
    ("/sbin", "usr/sbin"),
    ("/usr/lib64", "lib"),
    ("/usr/sbin", "bin"),
];

const DEFAULT_DIR_MODE: u32 = 0o040_000 + 0o755;
const DEFAULT_SYMLINK_MODE: u32 = 0o120_000;
const DEFAULT_FILE_MODE: u32 = 0o644;

pub struct Initramfs {
    entries: Vec<Entry>,
    files: HashSet<PathBuf>,
}

impl Initramfs {
    pub fn new(initramfs_type: InitramfsType, kroot: PathBuf, config: Config) -> Result<Initramfs> {
        let mut initramfs = Initramfs::new_basic_structure()?;
        initramfs.init(initramfs_type, kroot, config)?;
        Ok(initramfs)
    }

    fn init(
        &mut self,
        initramfs_type: InitramfsType,
        kroot: PathBuf,
        config: Config,
    ) -> Result<()> {
        let mut initrz: PathBuf =
            Path::new(&env::var("INITRZ").unwrap_or("target/release/initrz".to_string())).into();
        if !initrz.exists() {
            initrz = Path::new("/sbin/initrz").into();
            if !initrz.exists() {
                bail!("unable to find initrz executable. Please set INITRZ environment variable");
            }
        }
        self.add_elf_with_path(&initrz, Path::new("/init"))?;

        self.add_elf(Path::new("/sbin/vgchange"))?;
        self.add_elf(Path::new("/sbin/vgmknodes"))?;

        self.add_elf(Path::new("/bin/busybox"))?;

        let ld_conf = Path::new("/etc/ld.so.conf");
        self.add_entry(
            ld_conf,
            EntryBuilder::file(ld_conf, Vec::new())
                .with_metadata(&fs::metadata(&ld_conf)?)
                .build(),
        );

        self.add_file(&kroot.join("modules.dep"))?;
        self.add_file(&kroot.join("modules.alias"))?;

        self.apply_config(&config);

        initramfs_modules::get_modules(initramfs_type.clone(), &kroot, config.modules)?
            .iter()
            .try_for_each(|module| -> Result<()> {
                self.add_file(&module)?;
                Ok(())
            })?;

        match initramfs_type {
            InitramfsType::Host => {
                let crypttab = Path::new("/etc/crypttab.initramfs");
                if crypttab.exists() {
                    self.add_file(crypttab)?;
                }
            }
            InitramfsType::General => {}
        }

        Ok(())
    }

    fn new_basic_structure() -> Result<Initramfs> {
        let mut entries = Vec::new();
        let mut files: HashSet<PathBuf> = HashSet::new();

        ROOT_DIRECTORIES.iter().for_each(|dir| {
            files.insert((*dir).into());
            entries.push(EntryBuilder::directory(dir).mode(DEFAULT_DIR_MODE).build())
        });

        ROOT_SYMLINKS.iter().for_each(|(src, dest)| {
            files.insert((*src).into());
            entries.push(
                EntryBuilder::symlink(src, Path::new(dest))
                    .mode(DEFAULT_SYMLINK_MODE)
                    .build(),
            )
        });

        Ok(Initramfs { entries, files })
    }

    fn apply_config(&mut self, config: &Config) {}

    fn add_elf(&mut self, exe: &Path) -> Result<()> {
        self.add_elf_with_path(exe, exe)
    }

    fn add_elf_with_path(&mut self, exe: &Path, path: &Path) -> Result<()> {
        if !self.add_file_with_path(exe, path)? {
            return Ok(());
        }
        depend::resolve(Path::new(exe))?
            .iter()
            .try_for_each(|lib| -> Result<()> { self.add_library(lib) })?;

        Ok(())
    }

    fn add_library(&mut self, lib: &Path) -> Result<()> {
        let libname = lib.file_name().unwrap();
        if !self.add_file_with_path(lib, &Path::new("/usr/lib").join(libname))? {
            return Ok(());
        }

        depend::resolve(lib)?
            .iter()
            .try_for_each(|lib| -> Result<()> { self.add_library(lib) })?;

        Ok(())
    }

    fn add_file(&mut self, path: &Path) -> Result<bool> {
        self.add_file_with_path(path, path)
    }

    fn add_file_with_path(&mut self, file: &Path, path: &Path) -> Result<bool> {
        let file = fs::canonicalize(file)?;
        let path = path.to_path_buf();
        if self.files.contains(&path) {
            return Ok(false);
        }
        self.add_directory(
            path.parent()
                .expect("Files path shall contain a parent directory"),
        );
        self.add_entry(
            &path,
            EntryBuilder::file(
                &path,
                fs::read(&file).with_context(|| format!("unable to read from file {:?}", file))?,
            )
            .with_metadata(
                &fs::metadata(&file)
                    .with_context(|| format!("unable to read metadata of file {:?}", file))?,
            )
            .build(),
        );
        Ok(true)
    }

    fn add_directory(&mut self, dir: &Path) {
        if self.files.contains(dir) {
            return;
        }
        if let Some(parent) = dir.parent() {
            self.add_directory(parent);
        }

        self.add_entry(
            &dir,
            EntryBuilder::directory(&dir).mode(DEFAULT_DIR_MODE).build(),
        );
    }

    fn add_entry(&mut self, path: &Path, entry: Entry) {
        debug!("Added entry {:?}", path);
        self.files.insert(path.into());
        self.entries.push(entry);
    }

    pub fn into_bytes(self) -> Result<Vec<u8>> {
        Archive::new(self.entries).into_bytes()
    }
}
