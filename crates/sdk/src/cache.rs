//! Data computed once per column of cubes (heightmaps, biomes) and shared by every cube of that
//! column. Generators run on many threads at once, so the map is split into locked shards; a shard
//! that grows past its share of `capacity` is simply cleared.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SHARDS: usize = 32;

pub struct ColumnCache<T> {
    shards: [Mutex<HashMap<(u64, i32, i32), Arc<T>>>; SHARDS],
    per_shard: usize,
}

impl<T> ColumnCache<T> {
    pub fn new(capacity: usize) -> ColumnCache<T> {
        ColumnCache { shards: std::array::from_fn(|_| Mutex::new(HashMap::new())), per_shard: capacity.div_ceil(SHARDS).max(1) }
    }

    /// The value for column (`x`, `z`) in cube coordinates under `seed`, computing it with `make`
    /// on a miss. Two threads missing together may both compute; the result is the same.
    pub fn get(&self, seed: u64, x: i32, z: i32, make: impl FnOnce() -> T) -> Arc<T> {
        let key = (seed, x, z);
        let h = loom_noise::hash(0, x as i64, 0, z as i64) as u32;
        let shard = &self.shards[(h >> 27) as usize % SHARDS];
        if let Some(v) = shard.lock().unwrap().get(&key) {
            return v.clone();
        }
        let v = Arc::new(make());
        let mut map = shard.lock().unwrap();
        if map.len() >= self.per_shard {
            map.clear();
        }
        map.insert(key, v.clone());
        v
    }
}
