use std::path::Path;
use std::{collections::HashSet, env, fs};

use anyhow::{ensure, Context, Result};
use camino::Utf8Path;
use camino::Utf8PathBuf;
use colored::Colorize;
use log::debug;

use crate::config::Config;
use crate::depend;
use crate::initramfs_modules;
use crate::initramfs_type::InitramfsType;
use crate::newc::{Archive, Entry, EntryBuilder};

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

pub struct Initramfs {
    entries: Vec<Entry>,
    files: HashSet<Utf8PathBuf>,
}

impl Initramfs {
    pub fn new(
        initramfs_type: InitramfsType,
        kroot: Utf8PathBuf,
        config: Config,
    ) -> Result<Initramfs> {
        let mut initramfs = Initramfs::new_basic_structure()?;
        let initrz =
            Utf8PathBuf::from(&env::var("INITRZ").unwrap_or("target/release/initrz".to_string()));
        ensure!(
            initrz.exists(),
            "unable to find initrz executable. Please set INITRZ environment variable"
        );
        initramfs.add_elf_with_path(&initrz, Utf8Path::new("/init"))?;

        initramfs.add_elf(Utf8Path::new("/sbin/vgchange"))?;
        initramfs.add_elf(Utf8Path::new("/sbin/vgmknodes"))?;

        initramfs.add_elf(Utf8Path::new("/bin/busybox"))?;

        let ld_conf = Utf8Path::new("/etc/ld.so.conf");
        initramfs.add_entry(
            ld_conf,
            EntryBuilder::file(ld_conf, Vec::new())
                .with_metadata(&fs::metadata(ld_conf)?)
                .build(),
        );

        initramfs.add_file(&kroot.join("modules.dep"))?;
        initramfs.add_file(&kroot.join("modules.alias"))?;

        initramfs.apply_config(&config);

        initramfs_modules::get_modules(initramfs_type.clone(), &kroot, config.modules)?
            .iter()
            .try_for_each(|module| -> Result<()> {
                initramfs.add_file(module)?;
                Ok(())
            })?;

        match initramfs_type {
            InitramfsType::Host => {
                let crypttab = Utf8Path::new("/etc/crypttab.initramfs");
                if crypttab.exists() {
                    initramfs.add_file(crypttab)?;
                }
            }
            InitramfsType::General => {}
        }

        Ok(initramfs)
    }

    fn new_basic_structure() -> Result<Initramfs> {
        let mut entries = Vec::new();
        let mut files: HashSet<Utf8PathBuf> = HashSet::new();

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

    fn apply_config(&mut self, _config: &Config) {}

    fn add_elf(&mut self, exe: &Utf8Path) -> Result<()> {
        self.add_elf_with_path(exe, exe)
    }

    fn add_elf_with_path(&mut self, exe: &Utf8Path, path: &Utf8Path) -> Result<()> {
        if !self.add_file_with_path(exe, path)? {
            return Ok(());
        }
        depend::resolve(Utf8Path::new(exe))?
            .iter()
            .try_for_each(|lib| self.add_library(lib))?;

        Ok(())
    }

    fn add_library(&mut self, lib: &Utf8Path) -> Result<()> {
        let libname = lib.file_name().unwrap();
        const PATHS: [&str; 3] = ["/usr/lib/", "/usr/lib64", "/usr/local/lib"];
        let path = PATHS
            .iter()
            .find(|path| {
                let path = Utf8Path::new(path);
                path.join(libname).exists()
            })
            .with_context(|| format!("unable to find library {}", libname))?;
        if !self.add_file_with_path(lib, Utf8Path::new(path))? {
            return Ok(());
        }

        depend::resolve(Utf8Path::new(lib))?
            .iter()
            .try_for_each(|lib| self.add_library(lib))?;

        Ok(())
    }

    fn add_file(&mut self, path: &Utf8Path) -> Result<bool> {
        self.add_file_with_path(path, path)
    }

    fn add_file_with_path(&mut self, file: &Utf8Path, path: &Utf8Path) -> Result<bool> {
        ensure!(
            file.exists(),
            "file {} does not exist",
            file.as_str().red().bold()
        );
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

    fn add_directory(&mut self, dir: &Utf8Path) {
        if self.files.contains(dir) {
            return;
        }
        if let Some(parent) = dir.parent() {
            self.add_directory(parent);
        }

        self.add_entry(
            dir,
            EntryBuilder::directory(dir).mode(DEFAULT_DIR_MODE).build(),
        );
    }

    fn add_entry(&mut self, path: &Utf8Path, entry: Entry) {
        debug!("Added entry {:?}", path);
        self.files.insert(path.into());
        self.entries.push(entry);
    }

    pub fn into_bytes(self) -> Result<Vec<u8>> {
        Archive::new(self.entries).into_bytes()
    }
}
