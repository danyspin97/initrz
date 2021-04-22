mod config;
mod depend;
mod initramfs;
mod module;
mod modules;
mod newc;

use std::{ffi::OsString, fs::File, io::Write, path::Path};

use anyhow::Result;
use clap::Clap;
use log;
use simplelog::{ColorChoice, LevelFilter, TermLogger, TerminalMode};
use zstd::stream::write::Encoder;

use config::Config;
use initramfs::Initramfs;

#[derive(Clap)]
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
    #[clap(short = 'v', long = "verbose", parse(from_occurrences))]
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

    let config = Config::new(&Path::new(&opts.config))?;

    let initramfs = if opts.host {
        Initramfs::with_host_settings(&opts.kver, config)?
    } else {
        Initramfs::new(&opts.kver, config)?
    };
    let initramfs_file = File::create("initramfs.img")?;
    let mut zstd_encoder = Encoder::new(initramfs_file, 3)?;
    // zstd_encoder.multithread(1)?;
    zstd_encoder.write_all(&initramfs.into_bytes()?)?;
    zstd_encoder.finish()?;

    Ok(())
}
