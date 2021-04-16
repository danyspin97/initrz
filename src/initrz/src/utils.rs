use libblkid_rs::BlkidCache;
use log::warn;

use std::mem::MaybeUninit;
use std::path::Path;

pub fn get_blkid_cache() -> BlkidCache {
    let mut blkid_cache: BlkidCache = unsafe { MaybeUninit::zeroed().assume_init() };
    let res = blkid_cache.get_cache(Path::new("/blkid.cache"));
    if res.is_err() {
        warn!(
            "unable to load blkid cache from file /blkid.cache: {:?}",
            res.unwrap_err()
        );
    }

    blkid_cache
}
