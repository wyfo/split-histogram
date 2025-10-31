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

pub trait HistogramValue: Clone + Add<Output = Self> + PartialOrd + Sized {
    const MAX: Self;
    fn from_bits(bits: u64) -> Self;
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering);
}

impl HistogramValue for u64 {
    const MAX: Self = u64::MAX;
    fn from_bits(bits: u64) -> Self {
        bits
    }

    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter.fetch_add(value, ordering);
    }
}

impl HistogramValue for f64 {
    const MAX: Self = f64::INFINITY;
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

#[derive(Debug)]
pub struct Histogram<T = f64>(Arc<HistogramInner<T>>);

impl<T: HistogramValue> Histogram<T> {
    pub fn new(buckets: impl IntoIterator<Item = T>) -> Self {
        let buckets = buckets
            .into_iter()
            .chain(iter::once(T::MAX))
            .collect::<Vec<_>>();
        let shards = array::from_fn(|_| Shard::new(buckets.len()));
        Self(Arc::new(HistogramInner {
            buckets,
            hot_shard: AtomicU8::new(0),
            shards,
            collector: Mutex::new(()),
            waker: AtomicWaker::new(),
        }))
    }

    pub fn observe(&self, value: T) {
        let bucket_index = self.0.buckets.iter().position(|b| value <= *b);
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed) as usize;
        self.0.shards[hot_shard].observe(value, bucket_index, self.0.buckets.len(), &self.0.waker);
    }

    pub fn collect(&self) -> (u64, T, Vec<(T, u64)>) {
        let _guard = self.0.collector.lock().unwrap();
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed) as usize;
        let cold_shard = hot_shard ^ 1;
        let bucket_count = self.0.buckets.len();
        let (count_cold, sum_cold, buckets_cold) =
            self.0.shards[cold_shard].collect(bucket_count, &self.0.waker);
        self.0.hot_shard.store(cold_shard as u8, Ordering::Relaxed);
        let (count_hot, sum_hot, buckets_hot) =
            self.0.shards[hot_shard].collect(bucket_count, &self.0.waker);
        let buckets = (self.0.buckets.iter())
            .zip(iter::zip(buckets_cold, buckets_hot))
            .map(|(b, (cold, hot))| (b.clone(), cold + hot))
            .collect();
        (count_cold + count_hot, sum_cold + sum_hot, buckets)
    }
}

impl<T> Clone for Histogram<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Debug)]
struct HistogramInner<T> {
    buckets: Vec<T>,
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
            // `+ 3` for _count, _sum and `NaN` bucket
            counters: (0..(bucket_count + 3).div_ceil(COUNTERS_PER_CACHE_LINE))
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

    fn observe(
        &self,
        value: T,
        bucket_index: Option<usize>,
        bucket_count: usize,
        waker: &AtomicWaker,
    ) {
        self.bucket(bucket_index.unwrap_or(bucket_count))
            .fetch_add(1, Ordering::Relaxed);
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

    fn read_sum_and_buckets(&self, buckets: &mut [u64]) -> (T, u64) {
        let bucket_count = buckets.len();
        let sum = T::from_bits(self.sum().load(Ordering::Acquire));
        let mut expected_count = 0;
        for (count, counter) in buckets.iter_mut().zip(self.buckets(bucket_count)) {
            *count = counter.load(Ordering::Relaxed);
            expected_count += *count;
        }
        expected_count += self.bucket(bucket_count).load(Ordering::Relaxed);
        (sum, expected_count)
    }

    fn collect(&self, bucket_count: usize, waker: &AtomicWaker) -> (u64, T, Vec<u64>) {
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
    fn collect_cold(&self, buckets: &mut Vec<u64>, waker: &AtomicWaker) -> (u64, T, Vec<u64>) {
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
            let [(10, b0), (100, b1), (u64::MAX, b2)] = buckets[..] else {
                unreachable!()
            };
            assert_eq!(count, b0 + b1 + b2);
            let if_bucket = |b, v| if b == 1 { v } else { 0 };
            assert_eq!(
                sum,
                if_bucket(b0, 7) + if_bucket(b1, 42) + if_bucket(b2, 80100)
            );
            let (count, sum, buckets) = histogram.collect();
            assert_eq!(count, 3);
            assert_eq!(sum, 80149);
            assert_eq!(buckets, vec![(10, 1), (100, 1), (u64::MAX, 1)]);
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
                sum1 == 0 && sum2 == 42,
                std::sync::atomic::Ordering::Relaxed,
            );
            t1.join().unwrap();
            let (_, sum, _) = histogram.collect();
            assert_eq!(sum, 49);
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
