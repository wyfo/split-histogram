use std::{
    array, hint,
    hint::black_box,
    iter,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
};

use divan::Bencher;
use histogram::{Histogram, HistogramValue};

#[derive(Clone)]
struct GoHistogram(Arc<GoHistogramInner>);

struct GoHistogramInner {
    buckets: Vec<f64>,
    shard_idx_and_count: AtomicU64,
    shards: [GoShard; 2],
}

struct GoShard {
    count: AtomicU64,
    sum: AtomicU64,
    buckets: Vec<AtomicU64>,
}

impl GoHistogram {
    fn new(buckets: impl IntoIterator<Item = f64>) -> Self {
        let buckets = buckets
            .into_iter()
            .chain(iter::once(f64::INFINITY))
            .collect::<Vec<_>>();
        let shards = array::from_fn(|_| GoShard {
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            buckets: buckets.iter().map(|_| AtomicU64::new(0)).collect(),
        });
        Self(Arc::new(GoHistogramInner {
            buckets,
            shard_idx_and_count: AtomicU64::new(0),
            shards,
        }))
    }

    fn observe<const COUNT: bool>(&self, value: f64) {
        let bucket_idx = self.0.buckets.iter().position(|b| value <= *b).unwrap();
        let shard_idx = self.0.shard_idx_and_count.fetch_add(2, Ordering::Relaxed) & 1;
        let shard = &self.0.shards[shard_idx as usize];
        f64::atomic_add(&shard.sum, value, Ordering::Relaxed);
        shard.buckets[bucket_idx].fetch_add(
            1,
            if COUNT {
                Ordering::Relaxed
            } else {
                Ordering::Release
            },
        );
        if COUNT {
            shard.count.fetch_add(1, Ordering::Release);
        }
    }
}

const SPIN_LIMITS: &[Option<usize>] = &[None, Some(0), Some(1), Some(10), Some(100), Some(1000)];

fn bench<H: Sync>(
    bencher: Bencher,
    spin: Option<usize>,
    new: fn(Vec<f64>) -> H,
    observe: impl Fn(&H, f64) + Sync,
) {
    let histogram = new(vec![1.0]);
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        if let Some(spin) = spin.as_ref() {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..*spin {
                        hint::spin_loop();
                    }
                    observe(&histogram, black_box(1.0));
                }
            });
        }
        bencher.bench_local(|| observe(&histogram, black_box(0.5)));
        stop.store(true, Ordering::Relaxed);
    });
}

#[divan::bench(args = SPIN_LIMITS)]
fn observe(bencher: Bencher, spin: Option<usize>) {
    bench(bencher, spin, Histogram::new, Histogram::observe);
}

#[divan::bench(args = SPIN_LIMITS)]
fn go_observe(bencher: Bencher, spin: Option<usize>) {
    bench(
        bencher,
        spin,
        GoHistogram::new,
        GoHistogram::observe::<true>,
    );
}

#[divan::bench(args = SPIN_LIMITS)]
fn go_observe_no_count(bencher: Bencher, spin: Option<usize>) {
    bench(
        bencher,
        spin,
        GoHistogram::new,
        GoHistogram::observe::<false>,
    );
}

fn main() {
    divan::main();
}
