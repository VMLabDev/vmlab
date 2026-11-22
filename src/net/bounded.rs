//! A map with a size limit, for a table whose keys a guest writes (review
//! F6): a source address, an ARP sender, an echo destination. Past the limit
//! a new key replaces the key that was added first.
//!
//! First added, not least recently used: under a flood of new keys either
//! order drops every other entry within one pass of the table, and this one
//! costs no bookkeeping on a lookup. Whoever owned a dropped entry puts it
//! back with its next frame.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Debug)]
pub(crate) struct BoundedMap<K, V> {
    limit: usize,
    map: HashMap<K, V>,
    /// The keys of `map`, oldest first.
    order: VecDeque<K>,
}

impl<K: Copy + Eq + Hash, V> BoundedMap<K, V> {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Set `key` to `value`. A key the map already holds keeps its place in
    /// the order. A new key in a full map takes the place of the oldest.
    pub(crate) fn insert(&mut self, key: K, value: V) {
        if let Some(held) = self.map.get_mut(&key) {
            *held = value;
            return;
        }
        while self.map.len() >= self.limit {
            let Some(oldest) = self.order.pop_front() else {
                return;
            };
            self.map.remove(&oldest);
        }
        self.order.push_back(key);
        self.map.insert(key, value);
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.map.keys()
    }

    /// Read by the tests that hold a table to its limit.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_in_a_full_map_replaces_the_oldest() {
        let mut m = BoundedMap::new(2);
        m.insert(1, "a");
        m.insert(2, "b");
        // An update is not a new key: nothing is dropped, the order stays.
        m.insert(1, "a2");
        assert_eq!(m.len(), 2);
        m.insert(3, "c");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&1), None);
        assert_eq!(m.get(&2), Some(&"b"));
        assert_eq!(m.get(&3), Some(&"c"));
        assert!(m.contains_key(&2));
        assert_eq!(m.keys().count(), 2);
    }
}
