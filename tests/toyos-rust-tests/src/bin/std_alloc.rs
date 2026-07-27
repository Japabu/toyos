fn main() {
    // Vec
    let v: Vec<i32> = (0..1000).collect();
    assert_eq!(v.len(), 1000);
    assert_eq!(v[0], 0);
    assert_eq!(v[999], 999);
    assert_eq!(v.iter().sum::<i32>(), 499500);

    // String
    let mut s = String::new();
    for i in 0..100 {
        s.push_str(&format!("{i} "));
    }
    assert!(s.len() > 100);

    // Box
    let b = Box::new([0u8; 4096]);
    assert_eq!(b.len(), 4096);

    // BTreeMap (doesn't use TLS/RandomState unlike HashMap)
    let mut map = std::collections::BTreeMap::new();
    for i in 0..100 {
        map.insert(i, i * i);
    }
    assert_eq!(map.get(&10), Some(&100));
    assert_eq!(map.len(), 100);

    // Nested allocations
    let nested: Vec<Vec<u8>> = (0..10).map(|i| vec![i as u8; 100]).collect();
    assert_eq!(nested.len(), 10);
    assert_eq!(nested[5][0], 5);

    println!("all allocation tests passed");
}
