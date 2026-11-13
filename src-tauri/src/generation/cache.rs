//! Payload cache: encoded chunk payloads keyed by (pipeline revision,
//! chunk coord), so graph edits never serve stale data.

use std::sync::Arc;

use layergen::ChunkCoord;
pub use worldgraph::lru::ByteLru;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub revision: u64,
    pub coord: ChunkCoord,
}

pub type PayloadCache = ByteLru<CacheKey, Arc<Vec<u8>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_separate_keys() {
        let mut cache = PayloadCache::new(100);
        let old = CacheKey {
            revision: 0,
            coord: ChunkCoord(1, 0, 0),
        };
        let new = CacheKey { revision: 1, ..old };

        cache.insert(old, Arc::new(vec![0; 10]));

        assert!(cache.get(new).is_none());
        assert!(cache.get(old).is_some());
    }
}
