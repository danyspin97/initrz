mod config;
mod depend;
mod initramfs;
mod initramfs_modules;
mod initramfs_type;
mod newc;

use std::{
    ffi::OsString,
    fs::{self, File},
    io::Write,
    path::Path,
};

use anyhow::{Context, Result};
use clap::Parser;
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
    config: OsString,
    #[clap(short = 'h', long = "host-only")]
    host: bool,
    #[clap(short = 'k', long = "kver")]
    kver: String,
    #[clap(short = 'o', long = "output")]
    output: OsString,
    #[clap(short = 'q', long = "quiet")]
    quiet: bool,
    // TODO: Use from_occurences
    #[clap(short = 'v', long = "verbose")]
    verbose: u32,
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
        File::create(&opts.output)
            .with_context(|| format!("unable to create file {:?}", opts.output))?,
        3,
    )?;
    // zstd_encoder.multithread(1)?;
    zstd_encoder.write_all(
        &Initramfs::new(
            if opts.host {
                InitramfsType::Host
            } else {
                InitramfsType::General
            },
            // Canonicalize path to avoid problems with dowser and filter
            fs::canonicalize(Path::new("/lib/modules").join(&opts.kver))?,
            Config::new(Path::new(&opts.config))?,
        )?
        .into_bytes()?,
    )?;
    zstd_encoder.finish()?;

    Ok(())
}
