use super::*;

fn key(node: i64) -> RangeKey {
    RangeKey {
        node,
        version: "测试版本".into(),
        start: 0,
        len: RANGE_CACHE_ENTRY,
    }
}
fn value(bytes: Bytes) -> CachedRange {
    CachedRange {
        mime: "text/plain".into(),
        disposition: "inline".into(),
        bytes,
    }
}

#[test]
fn counters_distinguish_hits_misses_replacement_and_eviction() {
    let mut cache = RangeCache::default();
    assert!(cache.get(&key(1)).is_none());
    cache.put(key(1), value(Bytes::from_static(b"abc")));
    assert_eq!(cache.get(&key(1)).unwrap().bytes, "abc");
    cache.put(key(1), value(Bytes::from_static(b"ab")));
    assert_eq!(cache.budget, 2);
    assert_eq!(cache.evictions, 0);
    let bytes = Bytes::from(vec![0; RANGE_CACHE_ENTRY as usize]);
    for node in 2..=34 {
        cache.put(key(node), value(bytes.clone()));
    }
    assert_eq!(cache.evictions, 2);
    assert_eq!(cache.map.len(), 32);
    assert_eq!(cache.budget, RANGE_CACHE_BUDGET);
    let mut text = String::new();
    cache.write_metrics(&mut text);
    assert!(text.contains("tgdrive_range_cache_hits_total 1\n"));
    assert!(text.contains("tgdrive_range_cache_misses_total 1\n"));
    assert!(text.contains("tgdrive_range_cache_evictions_total 2\n"));
    assert!(text.contains("tgdrive_range_cache_entries 32\n"));
    assert!(!text.contains("测试版本"));
}
