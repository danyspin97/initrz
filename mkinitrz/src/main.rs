mod config;
mod depend;
mod initramfs;
mod initramfs_modules;
mod initramfs_type;
mod newc;

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{ensure, Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use colored::Colorize;
use simplelog::{ColorChoice, LevelFilter, TermLogger, TerminalMode};
use zstd::stream::write::Encoder;

use config::Config;
use initramfs::Initramfs;
use initramfs_type::InitramfsType;

#[derive(Clone, Copy, Debug)]
enum Compression {
    None,
    Zstd,
}

impl clap::ValueEnum for Compression {
    fn value_variants<'a>() -> &'a [Self] {
        &[Compression::None, Compression::Zstd]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        match self {
            Compression::None => Some(clap::builder::PossibleValue::new("none")),
            Compression::Zstd => Some(clap::builder::PossibleValue::new("zstd")),
        }
    }
}

#[derive(Parser)]
#[clap(version = "0.1", author = "danyspin97")]
struct Opts {
    #[clap(
        long = "config",
        default_value = "/etc/initrz/mkinitrz.conf"
    )]
    config: Utf8PathBuf,
    #[clap(long = "host-only")]
    host: bool,
    #[clap(short = 'k', long = "kver")]
    kernel_version: String,
    #[clap(short = 'o', long = "output")]
    output: Option<String>,
    #[clap(short = 'q', long = "quiet")]
    quiet: bool,
    #[clap(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
    #[clap(long, default_value = "/lib/modules")]
    kernel_modules_path: Utf8PathBuf,
    #[clap(value_enum, short, long, default_value_t = Compression::None)]
    compression: Compression,
}

fn main() -> Result<()> {
    let opts: Opts = Opts::parse();

    TermLogger::init(
        if opts.quiet {
            LevelFilter::Error
        } else {
            match opts.verbose {
                0 => LevelFilter::Warn,
                1 => LevelFilter::Info,
                2 => LevelFilter::Debug,
                _ => LevelFilter::Trace,
            }
        },
        simplelog::Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )?;

    let file = File::create(
        opts.output
            .clone()
            .unwrap_or_else(|| format!("initramfs-{}.img", opts.kernel_version)),
    )
    .with_context(|| format!("unable to create file {:?}", opts.output))?;

    ensure!(
        opts.kernel_modules_path.exists(),
        "{} is does not exists",
        opts.kernel_modules_path.as_str().red()
    );
    ensure!(
        opts.kernel_modules_path.is_dir(),
        "{} is not a directory",
        opts.kernel_modules_path.as_str().red()
    );
    let kernel_modules = opts.kernel_modules_path.join(&opts.kernel_version);
    // ensure that the path kernel_modules exists. If not, show the user all available kernel
    // versions
    ensure!(
        kernel_modules.exists(),
        "kernel version {} not found. Available versions: {}",
        opts.kernel_version.red(),
        opts.kernel_modules_path
            .read_dir()?
            .filter_map(|entry| {
                entry.ok().and_then(|entry| {
                    entry
                        .file_name()
                        .into_string()
                        .ok()
                        .map(|s| s.green().to_string())
                })
            })
            .collect::<Vec<String>>()
            .join(", ")
    );

    let initramfs = &Initramfs::new(
        if opts.host {
            InitramfsType::Host
        } else {
            InitramfsType::General
        },
        // Canonicalize path to avoid problems with dowser and filter
        Utf8PathBuf::from_path_buf(fs::canonicalize(kernel_modules)?).map_err(|path| {
            anyhow::anyhow!("unable to convert path {} to utf8", path.to_string_lossy())
        })?,
        Config::new(&opts.config)?,
    )?
    .into_bytes()?;

    let mut writer = BufWriter::new(file);

    match opts.compression {
        Compression::None => writer.write_all(initramfs)?,
        Compression::Zstd => {
            let mut zstd_encoder = Encoder::new(writer, 3)?;
            zstd_encoder.write_all(initramfs)?;
            zstd_encoder.finish()?;
        }
    }

    Ok(())
}
