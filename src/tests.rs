#[cfg(not(loom))]
use std::thread;

use itertools::Itertools;
#[cfg(loom)]
use loom::{model, thread};

use crate::Histogram;

#[cfg(not(loom))]
fn model(f: impl Fn()) {
    f();
}

#[test]
fn observe_and_collect() {
    model(move || {
        #[cfg(not(feature = "unsafe"))]
        let histogram = Histogram::new(&[10, 100]);
        #[cfg(feature = "unsafe")]
        let histogram = Histogram::new_trusted(&[10, 100]);
        let h1 = histogram.clone();
        let h2 = histogram.clone();
        let t1 = thread::spawn(move || h1.observe(42));
        let t2 = thread::spawn(move || {
            let (count, sum, buckets) = h2.collect();
            (count, sum, buckets.collect_vec())
        });
        histogram.observe(7);
        histogram.observe(80100);
        t1.join().unwrap();
        let (count, sum, buckets) = t2.join().unwrap();
        assert!(count <= 3);
        let [(10.0, b0), (100.0, b1), (f64::INFINITY, b2)] = buckets[..] else {
            unreachable!()
        };
        assert_eq!(count, b0 + b1 + b2);
        let if_bucket = |b, v| if b == 1 { v } else { 0.0 };
        assert_eq!(
            sum,
            if_bucket(b0, 7.0) + if_bucket(b1, 42.0) + if_bucket(b2, 80100.0)
        );
        let (count, sum, buckets) = histogram.collect();
        assert_eq!(count, 3);
        assert_eq!(sum, 80149.0);
        assert_eq!(
            buckets.collect_vec(),
            vec![(10.0, 1), (100.0, 1), (f64::INFINITY, 1)]
        );
    });
}

#[cfg(loom)]
#[test]
fn double_collect_edge_case() {
    let edge_case = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let edge_case2 = edge_case.clone();
    model(move || {
        let histogram = Histogram::new(vec![10, 100]);
        let h1 = histogram.clone();
        let t1 = thread::spawn(move || {
            h1.observe(7);
            h1.observe(42)
        });
        let (_, sum1, _) = histogram.collect();
        let (_, sum2, _) = histogram.collect();
        edge_case2.fetch_or(
            sum1 == 0.0 && sum2 == 42.0,
            std::sync::atomic::Ordering::Relaxed,
        );
        t1.join().unwrap();
        let (_, sum, _) = histogram.collect();
        assert_eq!(sum, 49.0);
    });
    assert!(edge_case.load(std::sync::atomic::Ordering::Relaxed));
}

#[cfg(not(loom))]
#[test]
fn observe_inf() {
    let histogram = Histogram::new(vec![1.0]);
    histogram.observe(f64::INFINITY);
    histogram.observe(1.0);
    let (count, sum, buckets) = histogram.collect();
    assert_eq!(count, 2);
    assert_eq!(sum, f64::INFINITY);
    assert_eq!(buckets.collect_vec(), vec![(1.0, 1), (f64::INFINITY, 1)]);
}

#[cfg(not(loom))]
#[test]
fn observe_nan() {
    let histogram = Histogram::new(vec![1.0]);
    histogram.observe(f64::NAN);
    let (count, sum, buckets) = histogram.collect();
    assert_eq!(count, 1);
    assert!(sum.is_nan());
    assert_eq!(buckets.collect_vec(), vec![(1.0, 0), (f64::INFINITY, 0)]);
}
