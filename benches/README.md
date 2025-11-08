# Benchmark

Benchmarking is hard, especially when it comes to concurrency. This benchmark focuses on **observation latency** under varying levels of contention, comparing three implementations:

- `go_observe`: Reference Go client algorithm (4 atomic RMW)
- `go_observe_no_count`: Go algorithm without `_count` increment (3 atomic RMW)
- `observe`: This algorithm (3 atomic RMW + cache locality)

The benchmark is parameterized with an optional `spin` value. When not `None`, a background thread performs continuous observations interleaved with `std::hint::spin_loop`[^1] called `spin` times.

## Results

### `ubuntu-24.04` (x86-64)[^2]
```
Timer precision: 15 ns
comparison              fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ go_observe                         │               │               │               │         │
│  ├─ None              27.2 ns       │ 634.6 ns      │ 27.32 ns      │ 27.49 ns      │ 486604  │ 31142656
│  ├─ Some(0)           27.26 ns      │ 725.2 ns      │ 218 ns        │ 218.5 ns      │ 70100   │ 4486400
│  ├─ Some(1)           27.26 ns      │ 762.4 ns      │ 212.5 ns      │ 211.9 ns      │ 72258   │ 4624512
│  ├─ Some(2)           27.28 ns      │ 887.8 ns      │ 202.7 ns      │ 202.3 ns      │ 75596   │ 4838144
│  ├─ Some(4)           27.26 ns      │ 2.184 µs      │ 203 ns        │ 192.8 ns      │ 79169   │ 5066816
│  ├─ Some(8)           27.28 ns      │ 13.66 µs      │ 124.1 ns      │ 125.9 ns      │ 119512  │ 7648768
│  ├─ Some(64)          27.26 ns      │ 530.5 ns      │ 37.65 ns      │ 38.16 ns      │ 367199  │ 23500736
│  ╰─ Some(1024)        27.2 ns       │ 338.2 ns      │ 27.34 ns      │ 28.24 ns      │ 478825  │ 30644800
├─ go_observe_no_count                │               │               │               │         │
│  ├─ None              21.46 ns      │ 527.5 ns      │ 21.78 ns      │ 21.93 ns      │ 308587  │ 39499136
│  ├─ Some(0)           21.79 ns      │ 613.8 ns      │ 151 ns        │ 149 ns        │ 101782  │ 6514048
│  ├─ Some(1)           21.79 ns      │ 1.95 µs       │ 162.2 ns      │ 163.6 ns      │ 93007   │ 5952448
│  ├─ Some(2)           21.78 ns      │ 630.7 ns      │ 169.4 ns      │ 170.9 ns      │ 89120   │ 5703680
│  ├─ Some(4)           21.81 ns      │ 446.2 ns      │ 138.9 ns      │ 139.4 ns      │ 108660  │ 6954240
│  ├─ Some(8)           21.78 ns      │ 610.8 ns      │ 84.57 ns      │ 85.84 ns      │ 173218  │ 11085952
│  ├─ Some(64)          21.84 ns      │ 1.063 µs      │ 28.13 ns      │ 28.66 ns      │ 474728  │ 30382592
│  ╰─ Some(1024)        21.65 ns      │ 553.4 ns      │ 21.96 ns      │ 22.58 ns      │ 578841  │ 37045824
╰─ observe                            │               │               │               │         │
   ├─ None              19.01 ns      │ 219.3 ns      │ 19.26 ns      │ 19.33 ns      │ 349330  │ 44714240
   ├─ Some(0)           19.18 ns      │ 723.2 ns      │ 101.2 ns      │ 101.4 ns      │ 147809  │ 9459776
   ├─ Some(1)           19.2 ns       │ 528.4 ns      │ 103.9 ns      │ 103.7 ns      │ 144776  │ 9265664
   ├─ Some(2)           19.08 ns      │ 286.9 ns      │ 97.25 ns      │ 96.95 ns      │ 78034   │ 9988352
   ├─ Some(4)           19.04 ns      │ 443.6 ns      │ 62.7 ns       │ 63.14 ns      │ 117845  │ 15084160
   ├─ Some(8)           19.03 ns      │ 189.3 ns      │ 40.78 ns      │ 41.16 ns      │ 176351  │ 22572928
   ├─ Some(64)          19.18 ns      │ 558.7 ns      │ 22.06 ns      │ 22.34 ns      │ 589850  │ 37750400
   ╰─ Some(1024)        19.03 ns      │ 204.7 ns      │ 19.26 ns      │ 19.5 ns       │ 346030  │ 44291840
```

### `ubuntu-24.04-arm` (aarch64)[^2]
```
Timer precision: 24 ns
comparison              fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ go_observe                         │               │               │               │         │
│  ├─ None              20.82 ns      │ 380.8 ns      │ 21.07 ns      │ 21.13 ns      │ 332182  │ 42519296
│  ├─ Some(0)           21.01 ns      │ 644.7 ns      │ 368.4 ns      │ 356.5 ns      │ 21776   │ 2787328
│  ├─ Some(1)           20.95 ns      │ 527.3 ns      │ 364.7 ns      │ 362.1 ns      │ 21439   │ 2744192
│  ├─ Some(2)           21.01 ns      │ 699.4 ns      │ 298.5 ns      │ 295.9 ns      │ 26203   │ 3353984
│  ├─ Some(4)           21.01 ns      │ 1.606 µs      │ 371.9 ns      │ 349.5 ns      │ 22210   │ 2842880
│  ├─ Some(8)           21.01 ns      │ 2.848 µs      │ 158.8 ns      │ 166.7 ns      │ 46233   │ 5917824
│  ├─ Some(64)          20.95 ns      │ 1.001 µs      │ 40.82 ns      │ 40.68 ns      │ 181952  │ 23289856
│  ╰─ Some(1024)        20.7 ns       │ 388 ns        │ 21.14 ns      │ 22.12 ns      │ 320441  │ 41016448
├─ go_observe_no_count                │               │               │               │         │
│  ├─ None              16.29 ns      │ 177.3 ns      │ 16.86 ns      │ 16.88 ns      │ 208365  │ 53341440
│  ├─ Some(0)           16.73 ns      │ 655.8 ns      │ 357.4 ns      │ 319.6 ns      │ 12152   │ 3110912
│  ├─ Some(1)           16.82 ns      │ 432.7 ns      │ 309.4 ns      │ 304.8 ns      │ 12739   │ 3261184
│  ├─ Some(2)           16.39 ns      │ 691.3 ns      │ 278.6 ns      │ 277.1 ns      │ 13999   │ 3583744
│  ├─ Some(4)           16.67 ns      │ 439.4 ns      │ 314.9 ns      │ 302.8 ns      │ 12820   │ 3281920
│  ├─ Some(8)           16.79 ns      │ 530.8 ns      │ 99.17 ns      │ 100.7 ns      │ 38074   │ 9746944
│  ├─ Some(64)          16.82 ns      │ 523 ns        │ 28.61 ns      │ 28.66 ns      │ 127945  │ 32753920
│  ╰─ Some(1024)        16.29 ns      │ 449.8 ns      │ 17.26 ns      │ 17.49 ns      │ 201792  │ 51658752
╰─ observe                            │               │               │               │         │
   ├─ None              15.67 ns      │ 467.2 ns      │ 16.01 ns      │ 16.01 ns      │ 218270  │ 55877120
   ├─ Some(0)           15.76 ns      │ 242.8 ns      │ 80.79 ns      │ 80.73 ns      │ 47297   │ 12108032
   ├─ Some(1)           15.73 ns      │ 458.1 ns      │ 120 ns        │ 116.6 ns      │ 32978   │ 8442368
   ├─ Some(2)           16.01 ns      │ 379.6 ns      │ 60.82 ns      │ 61.01 ns      │ 62141   │ 15908096
   ├─ Some(4)           15.82 ns      │ 239.8 ns      │ 75.36 ns      │ 71.33 ns      │ 53379   │ 13665024
   ├─ Some(8)           15.89 ns      │ 383.8 ns      │ 51.54 ns      │ 51.65 ns      │ 72971   │ 18680576
   ├─ Some(64)          15.86 ns      │ 199.2 ns      │ 21.14 ns      │ 21.33 ns      │ 168309  │ 43087104
   ╰─ Some(1024)        15.67 ns      │ 239.2 ns      │ 16.07 ns      │ 16.34 ns      │ 214428  │ 54893568
```

### MacBook Air M3 (aarch64)
```
Timer precision: 41 ns
comparison              fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ go_observe                         │               │               │               │         │
│  ├─ None              4.2 ns        │ 39.07 ns      │ 4.282 ns      │ 4.299 ns      │ 167696  │ 171720704
│  ├─ Some(0)           4.363 ns      │ 206.5 ns      │ 134.8 ns      │ 130.3 ns      │ 7412    │ 7589888
│  ├─ Some(1)           4.811 ns      │ 159.6 ns      │ 110.6 ns      │ 119.7 ns      │ 8061    │ 8254464
│  ├─ Some(2)           4.81 ns       │ 272.6 ns      │ 224.3 ns      │ 215.9 ns      │ 4493    │ 4600832
│  ├─ Some(4)           9.856 ns      │ 161.2 ns      │ 85.66 ns      │ 80.67 ns      │ 11901   │ 12186624
│  ├─ Some(8)           4.363 ns      │ 85.74 ns      │ 20.43 ns      │ 20.82 ns      │ 43984   │ 45039616
│  ├─ Some(64)          4.769 ns      │ 25.48 ns      │ 5.787 ns      │ 5.781 ns      │ 137053  │ 140342272
│  ╰─ Some(1024)        4.322 ns      │ 18.23 ns      │ 4.689 ns      │ 4.682 ns      │ 161029  │ 164893696
├─ go_observe_no_count                │               │               │               │         │
│  ├─ None              4.119 ns      │ 13.51 ns      │ 4.241 ns      │ 4.348 ns      │ 171530  │ 175646720
│  ├─ Some(0)           4.729 ns      │ 3.035 µs      │ 94.28 ns      │ 97.19 ns      │ 9901    │ 10138624
│  ├─ Some(1)           4.322 ns      │ 124.8 ns      │ 80.77 ns      │ 83.91 ns      │ 11439   │ 11713536
│  ├─ Some(2)           4.322 ns      │ 111.2 ns      │ 75.85 ns      │ 73.39 ns      │ 13056   │ 13369344
│  ├─ Some(4)           4.282 ns      │ 101.8 ns      │ 30.32 ns      │ 41.17 ns      │ 22927   │ 23477248
│  ├─ Some(8)           4.322 ns      │ 34.84 ns      │ 11.85 ns      │ 11.74 ns      │ 74222   │ 76003328
│  ├─ Some(64)          4.363 ns      │ 39.96 ns      │ 5.381 ns      │ 5.393 ns      │ 143923  │ 147377152
│  ╰─ Some(1024)        4.241 ns      │ 37.85 ns      │ 4.688 ns      │ 4.661 ns      │ 161031  │ 164895744
╰─ observe                            │               │               │               │         │
   ├─ None              4.241 ns      │ 18.84 ns      │ 4.648 ns      │ 4.624 ns      │ 162579  │ 166480896
   ├─ Some(0)           4.403 ns      │ 97.68 µs      │ 10.54 ns      │ 410.4 ns      │ 2405    │ 2462720
   ├─ Some(1)           4.403 ns      │ 29.29 µs      │ 22.99 ns      │ 990.1 ns      │ 993     │ 1016832
   ├─ Some(2)           4.403 ns      │ 12.14 µs      │ 27.19 ns      │ 43.83 ns      │ 21608   │ 22126592
   ├─ Some(4)           4.363 ns      │ 37.97 ns      │ 16.65 ns      │ 17.82 ns      │ 50943   │ 52165632
   ├─ Some(8)           4.363 ns      │ 70.85 ns      │ 11.11 ns      │ 11.26 ns      │ 77296   │ 79151104
   ├─ Some(64)          4.404 ns      │ 39.07 ns      │ 5.624 ns      │ 5.577 ns      │ 139968  │ 143327232
   ╰─ Some(1024)        4.363 ns      │ 44.07 ns      │ 4.77 ns       │ 4.764 ns      │ 158368  │ 162168832
```

## Analysis

The additional atomic RMW in `go_observe` (the `_count` increment) has a measurable cost across all platforms, with the sole exception of Apple M3 in uncontended scenario.

Cache locality, enabled by grouping all shard counters in a single cache line, delivers consistent performance improvements across all platforms, significantly reducing the impact of cache line invalidation triggered by the contending thread.

## Cache-Friendly vs. Naive Layout

A "naive" shard layout — where buckets are not grouped with `_count` and `_sum` — can be enabled via the `naive` feature flag. Its performance matches `go_observe_no_count`, confirming the significant impact of cache locality.

[^1]: On a MacBook Air M3, one `std::hint::spin_loop` call takes ~8 ns.
[^2]: GitHub Actions workflow run: https://github.com/wyfo/split-histogram/actions/runs/18954432694
