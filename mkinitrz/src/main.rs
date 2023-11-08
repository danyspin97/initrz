mod config;
mod depend;
mod initramfs;
mod initramfs_modules;
mod initramfs_type;
mod newc;

use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

use anyhow::{ensure, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use colored::Colorize;
use simplelog::{ColorChoice, LevelFilter, TermLogger, TerminalMode};
use zstd::stream::write::Encoder;

use config::Config;
use initramfs::Initramfs;
use initramfs_type::InitramfsType;

#[derive(Parser)]
#[clap(version = "0.1", author = "danyspin97")]
struct Opts {
    #[clap(
        short = 'c',
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

    let mut zstd_encoder = Encoder::new(
        File::create(
            opts.output
                .clone()
                .unwrap_or_else(|| format!("initramfs-{}.img", opts.kernel_version)),
        )
        .with_context(|| format!("unable to create file {:?}", opts.output))?,
        3,
    )?;

    let kernel_modules = Path::new("/lib/modules").join(&opts.kernel_version);
    // ensure that the path kernel_modules exists. If not, show the user all available kernel
    // versions
    ensure!(
        kernel_modules.exists(),
        "kernel version {} not found. Available versions: {}",
        opts.kernel_version.red(),
        Utf8Path::new("/lib/modules")
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

    zstd_encoder.write_all(
        &Initramfs::new(
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
        .into_bytes()?,
    )?;
    zstd_encoder.finish()?;

    Ok(())
}
