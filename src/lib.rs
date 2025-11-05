#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::{
    array,
    future::poll_fn,
    iter,
    marker::PhantomData,
    mem,
    ops::Add,
    sync::{Arc, Mutex},
    task::Poll,
};

use crossbeam_utils::CachePadded;
#[cfg(not(loom))]
use futures_executor::block_on;
#[cfg(not(loom))]
use futures_util::task::AtomicWaker;
#[cfg(loom)]
use loom::{
    future::{block_on, AtomicWaker},
    sync::atomic::{AtomicU64, AtomicU8, Ordering},
};

pub trait HistogramValue: Add<Output = Self> + PartialOrd + Sized {
    const HAS_NAN: bool;
    fn into_f64(self) -> f64;
    fn is_nan(&self) -> bool;
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering);
    fn from_bits(bits: u64) -> Self;
}

impl HistogramValue for u64 {
    const HAS_NAN: bool = false;
    fn into_f64(self) -> f64 {
        self as f64
    }
    fn is_nan(&self) -> bool {
        false
    }
    fn from_bits(bits: u64) -> Self {
        bits
    }
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter.fetch_add(value, ordering);
    }
}

impl HistogramValue for f64 {
    const HAS_NAN: bool = true;
    fn into_f64(self) -> f64 {
        self
    }
    fn is_nan(&self) -> bool {
        f64::is_nan(*self)
    }
    fn from_bits(bits: u64) -> Self {
        f64::from_bits(bits)
    }
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter
            .fetch_update(ordering, Ordering::Relaxed, |c| {
                Some(f64::to_bits(f64::from_bits(c) + value))
            })
            .unwrap();
    }
}

#[expect(clippy::len_without_is_empty)]
pub trait HistogramBuckets<T> {
    fn len(&self) -> usize;
    fn bucket_index(&self, value: &T) -> Option<usize>;
    fn iter(&self) -> impl Iterator<Item = f64> + '_;
}

impl<T: HistogramValue + Clone + 'static, B: AsRef<[T]>> HistogramBuckets<T> for B {
    fn len(&self) -> usize {
        self.as_ref().len()
    }

    fn bucket_index(&self, value: &T) -> Option<usize> {
        self.as_ref().iter().position(|b| value <= b)
    }

    fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.as_ref().iter().cloned().map(T::into_f64)
    }
}

#[derive(Debug)]
pub struct Histogram<T = f64, B = Vec<T>>(Arc<HistogramInner<T, B>>);

impl<T: HistogramValue, B: HistogramBuckets<T>> Histogram<T, B> {
    fn bucket_count(buckets: &B) -> usize {
        buckets.len() + /* inf */ 1 + /* nan */ if T::HAS_NAN { 1 } else { 0 }
    }

    pub fn new(buckets: B) -> Self {
        let shards = array::from_fn(|_| Shard::new(Self::bucket_count(&buckets)));
        Self(Arc::new(HistogramInner {
            buckets,
            hot_shard: AtomicU8::new(0),
            shards,
            collector: Mutex::new(()),
            waker: AtomicWaker::new(),
        }))
    }

    pub fn observe(&self, value: T) {
        let buckets = &self.0.buckets;
        let fallback_bucket = || buckets.len() + if value.is_nan() { 1 } else { 0 };
        let bucket_index = buckets.bucket_index(&value).unwrap_or_else(fallback_bucket);
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed) as usize;
        self.0.shards[hot_shard].observe(value, bucket_index, &self.0.waker);
    }

    pub fn collect(&self) -> (u64, f64, Vec<(f64, u64)>) {
        let _guard = self.0.collector.lock().unwrap();
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed) as usize;
        let cold_shard = hot_shard ^ 1;
        let bucket_count = Self::bucket_count(&self.0.buckets);
        let (count_cold, sum_cold, buckets_cold) =
            self.0.shards[cold_shard].collect(bucket_count, &self.0.waker);
        self.0.hot_shard.store(cold_shard as u8, Ordering::Relaxed);
        let (count_hot, sum_hot, buckets_hot) =
            self.0.shards[hot_shard].collect(bucket_count, &self.0.waker);
        let buckets = (self.0.buckets.iter().chain([f64::INFINITY]))
            .zip(iter::zip(buckets_cold, buckets_hot))
            .map(|(b, (cold, hot))| (b, cold + hot))
            .collect();
        (count_cold + count_hot, sum_cold + sum_hot, buckets)
    }
}

impl<T, B> Clone for Histogram<T, B> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Debug)]
struct HistogramInner<T, B> {
    buckets: B,
    hot_shard: AtomicU8,
    shards: [Shard<T>; 2],
    collector: Mutex<()>,
    waker: AtomicWaker,
}

const COUNTERS_PER_CACHE_LINE: usize = align_of::<CachePadded<()>>() / align_of::<AtomicU64>();
#[cfg(not(loom))]
const SPIN_LOOP_LIMIT: usize = 10;
#[cfg(loom)]
const SPIN_LOOP_LIMIT: usize = 1;
const WAITING_FLAG: u64 = 1 << (u64::BITS - 1);
const COUNT_MASK: u64 = !WAITING_FLAG;

#[derive(Debug)]
struct Shard<T> {
    // all counters should be stored on the same cache line to optimize grouped atomic operations
    // they are stored in the following order:
    // - _count
    // - _sum
    // - _buckets
    counters: Vec<CachePadded<[AtomicU64; COUNTERS_PER_CACHE_LINE]>>,
    _phantom: PhantomData<T>,
}

impl<T: HistogramValue> Shard<T> {
    fn new(bucket_count: usize) -> Self {
        Self {
            counters: (0..(/* _count + _sum */2 + bucket_count).div_ceil(COUNTERS_PER_CACHE_LINE))
                .map(|_| CachePadded::new(array::from_fn(|_| AtomicU64::new(0))))
                .collect(),
            _phantom: PhantomData,
        }
    }

    fn count(&self) -> &AtomicU64 {
        &self.counters[0][0]
    }

    fn sum(&self) -> &AtomicU64 {
        &self.counters[0][1]
    }

    fn bucket(&self, bucket_index: usize) -> &AtomicU64 {
        let idx = bucket_index + 2;
        &self.counters[idx / COUNTERS_PER_CACHE_LINE][idx % COUNTERS_PER_CACHE_LINE]
    }

    fn buckets(&self, bucket_count: usize) -> impl IntoIterator<Item = &AtomicU64> {
        self.counters
            .iter()
            .flat_map(|cache_line| cache_line.iter())
            .skip(2)
            .take(bucket_count)
    }

    fn observe(&self, value: T, bucket_index: usize, waker: &AtomicWaker) {
        self.bucket(bucket_index).fetch_add(1, Ordering::Relaxed);
        T::atomic_add(self.sum(), value, Ordering::Release);
        let count = self.count().fetch_add(1, Ordering::Release);
        if count & WAITING_FLAG != 0 {
            #[cold]
            fn wake(waker: &AtomicWaker) {
                waker.wake();
            }
            wake(waker);
        }
    }

    fn read_sum_and_buckets(&self, buckets: &mut [u64]) -> (f64, u64) {
        let bucket_count = buckets.len();
        let sum = T::from_bits(self.sum().load(Ordering::Acquire)).into_f64();
        let mut expected_count = 0;
        for (count, counter) in buckets.iter_mut().zip(self.buckets(bucket_count)) {
            *count = counter.load(Ordering::Relaxed);
            expected_count += *count;
        }
        (sum, expected_count)
    }

    fn collect(&self, bucket_count: usize, waker: &AtomicWaker) -> (u64, f64, Vec<u64>) {
        let mut buckets = vec![0; bucket_count];
        for _ in 0..SPIN_LOOP_LIMIT {
            let count = self.count().load(Ordering::Acquire) & COUNT_MASK;
            let (sum, expected_count) = self.read_sum_and_buckets(&mut buckets);
            if count == expected_count {
                return (count, sum, buckets);
            }
        }
        self.collect_cold(&mut buckets, waker)
    }

    #[cold]
    fn collect_cold(&self, buckets: &mut Vec<u64>, waker: &AtomicWaker) -> (u64, f64, Vec<u64>) {
        block_on(poll_fn(move |cx| {
            #[cfg(not(loom))]
            waker.register(cx.waker());
            #[cfg(loom)]
            waker.register(cx.waker().clone());
            let count = self.count().fetch_or(WAITING_FLAG, Ordering::Acquire) & COUNT_MASK;
            let (sum, expected_count) = self.read_sum_and_buckets(buckets);
            if count == expected_count {
                if self.count().fetch_and(COUNT_MASK, Ordering::Relaxed) & WAITING_FLAG != 0 {
                    #[cfg(not(loom))]
                    waker.take();
                    #[cfg(loom)]
                    waker.take_waker();
                }
                return Poll::Ready((count, sum, mem::take(buckets)));
            }
            Poll::Pending
        }))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(loom))]
    use std::thread;

    #[cfg(loom)]
    use loom::{model, thread};

    use crate::Histogram;

    #[cfg(not(loom))]
    fn model(f: impl Fn()) {
        f();
    }

    #[test]
    fn observe_and_collect() {
        model(|| {
            let histogram = Histogram::new([10, 100]);
            let h1 = histogram.clone();
            let h2 = histogram.clone();
            let t1 = thread::spawn(move || h1.observe(42));
            let t2 = thread::spawn(move || h2.collect());
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
            assert_eq!(buckets, vec![(10.0, 1), (100.0, 1), (f64::INFINITY, 1)]);
        });
    }

    #[cfg(loom)]
    #[test]
    fn double_collect_edge_case() {
        let edge_case = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let edge_case2 = edge_case.clone();
        model(move || {
            let histogram = Histogram::new([10, 100]);
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
        let histogram = Histogram::new([1.0]);
        histogram.observe(f64::INFINITY);
        assert_eq!(
            histogram.collect(),
            (1, f64::INFINITY, vec![(1.0, 0), (f64::INFINITY, 1)])
        );
    }

    #[cfg(not(loom))]
    #[test]
    fn observe_nan() {
        let histogram = Histogram::new([1.0]);
        histogram.observe(f64::NAN);
        let (count, sum, buckets) = histogram.collect();
        assert_eq!(count, 1);
        assert!(sum.is_nan());
        assert_eq!(buckets, vec![(1.0, 0), (f64::INFINITY, 0)]);
    }
}
