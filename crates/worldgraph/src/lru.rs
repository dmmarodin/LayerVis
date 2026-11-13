//! Byte-bounded LRU cache (used for the neighborhood prefix cache here and
//! for payload caches in LayerVis).

use std::{
    collections::{BTreeMap, HashMap},
    hash::Hash,
    sync::Arc,
};

/// Approximate heap size used for the byte budget.
pub trait Weigh {
    fn weight(&self) -> usize;
}

impl Weigh for Arc<Vec<u8>> {
    fn weight(&self) -> usize {
        self.len()
    }
}

struct Entry<V> {
    value: V,
    last_used: u64,
}

pub struct ByteLru<K, V> {
    max_bytes: usize,
    used_bytes: usize,
    tick: u64,
    entries: HashMap<K, Entry<V>>,
    /// last_used -> key, oldest first.
    order: BTreeMap<u64, K>,
    pub hits: u64,
    pub misses: u64,
}

impl<K: Copy + Eq + Hash, V: Clone + Weigh> ByteLru<K, V> {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            used_bytes: 0,
            tick: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    pub fn get(&mut self, key: K) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;

        let Some(entry) = self.entries.get_mut(&key) else {
            self.misses += 1;
            return None;
        };

        self.order.remove(&entry.last_used);
        entry.last_used = tick;
        self.order.insert(tick, key);
        self.hits += 1;

        Some(entry.value.clone())
    }

    pub fn insert(&mut self, key: K, value: V) {
        let weight = value.weight();
        if weight > self.max_bytes {
            return;
        }

        self.tick += 1;
        let tick = self.tick;

        if let Some(old) = self.entries.remove(&key) {
            self.order.remove(&old.last_used);
            self.used_bytes -= old.value.weight();
        }

        self.used_bytes += weight;
        self.entries.insert(
            key,
            Entry {
                value,
                last_used: tick,
            },
        );
        self.order.insert(tick, key);

        while self.used_bytes > self.max_bytes {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };

            if let Some(evicted) = self.entries.remove(&oldest) {
                self.used_bytes -= evicted.value.weight();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(len: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0; len])
    }

    #[test]
    fn evicts_least_recently_used_within_byte_budget() {
        let mut cache = ByteLru::new(30);

        cache.insert(1, bytes(10));
        cache.insert(2, bytes(10));
        cache.insert(3, bytes(10));

        // Touch 1 so 2 becomes the oldest.
        assert!(cache.get(1).is_some());

        cache.insert(4, bytes(10));

        assert!(cache.get(2).is_none());
        assert!(cache.get(1).is_some());
        assert!(cache.get(3).is_some());
        assert!(cache.get(4).is_some());
        assert_eq!(cache.used_bytes(), 30);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn replacing_a_key_does_not_double_count_and_oversized_is_skipped() {
        let mut cache = ByteLru::new(25);

        cache.insert(1, bytes(10));
        cache.insert(1, bytes(20));
        assert_eq!(cache.used_bytes(), 20);
        assert_eq!(cache.len(), 1);

        cache.insert(2, bytes(26));
        assert!(cache.get(2).is_none());
        assert_eq!(cache.used_bytes(), 20);
    }
}
