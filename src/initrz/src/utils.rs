use libblkid_rs::BlkidCache;

use std::path::Path;

pub fn get_blkid_cache() -> BlkidCache {
    BlkidCache::get_cache(Some(Path::new("/blkid.cache"))).expect("Failed to get BLKID cache")
}
