use anyhow::{Context, Result};
use libblkid_rs::BlkidCache;

use std::path::Path;

pub fn get_blkid_cache() -> Result<BlkidCache> {
    BlkidCache::get_cache(Some(Path::new("/blkid.cache"))).context("failed to get blkid cache")
}
