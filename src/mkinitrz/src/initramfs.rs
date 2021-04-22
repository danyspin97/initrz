use std::{
    collections::HashSet,
    convert::TryInto,
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::Result;
use log::debug;

use crate::config::Config;
use crate::depend;
use crate::module::Module;
use crate::modules;
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
    pub fn new(kver: &str, config: Config) -> Result<Initramfs> {
        let mut initramfs = Initramfs::new_common_structure(kver, &config)?;

        let modules = modules::get_general_modules(kver, config.modules)?;
        initramfs.add_modules(kver, modules)?;

        Ok(initramfs)
    }

    pub fn with_host_settings(kver: &str, config: Config) -> Result<Initramfs> {
        let mut initramfs = Initramfs::new_common_structure(kver, &config)?;

        let crypttab = Path::new("/etc/crypttab.initramfs");
        if crypttab.exists() {
            initramfs.add_file(crypttab)?;
        }

        let modules = modules::get_host_modules(kver, config.modules)?;
        initramfs.add_modules(kver, modules)?;

        Ok(initramfs)
    }

    fn new_common_structure(kver: &str, config: &Config) -> Result<Initramfs> {
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

        let mut initramfs = Initramfs { entries, files };

        let mut initrz: PathBuf =
            Path::new(&env::var("INITRZ").unwrap_or("target/release/initrz".to_string())).into();
        if !initrz.exists() {
            initrz = Path::new("/sbin/initrz").into();
        }
        initramfs.add_elf_with_path(&initrz, Path::new("/init"))?;

        initramfs.add_elf(Path::new("/sbin/vgchange"))?;
        initramfs.add_elf(Path::new("/sbin/vgmknodes"))?;

        initramfs.add_elf(Path::new("/bin/busybox"))?;

        let ld_conf = Path::new("/etc/ld.so.conf");
        initramfs.add_entry(
            ld_conf,
            EntryBuilder::file(ld_conf, Vec::new())
                .with_metadata(&fs::metadata(&ld_conf)?)
                .build(),
        );

        let kernel_root = Path::new("/lib/modules").join(kver);
        initramfs.add_file(&kernel_root.join("modules.dep"))?;
        initramfs.add_file(&kernel_root.join("modules.alias"))?;

        initramfs.apply_config(config);

        Ok(initramfs)
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
            EntryBuilder::file(&path, fs::read(&file)?)
                .with_metadata(&fs::metadata(file)?)
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

    fn add_modules(&mut self, kver: &str, modules: Vec<Module>) -> Result<()> {
        Ok(modules.iter().try_for_each(|module| -> Result<()> {
            let path = &module.path.with_extension("");
            self.add_directory(path.parent().unwrap());
            Ok(self.add_entry(
                &path,
                EntryBuilder::file(&path, module.into_bytes()?)
                    .with_metadata(&fs::metadata(&module.path)?)
                    .build(),
            ))
        })?)
    }

    pub fn into_bytes(self) -> Result<Vec<u8>> {
        Archive::new(self.entries).into_bytes()
    }
}
