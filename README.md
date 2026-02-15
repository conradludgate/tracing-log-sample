# tracing-log-sample

A [`tracing-subscriber`] layer that rate-limits log output using [reservoir sampling].

Instead of dropping logs above a threshold or probabilistically skipping them up-front,
this layer collects events into fixed-duration time buckets and applies Algorithm R
to produce a statistically uniform sample per bucket.

## Features

- **Cascading budgets** — define multiple sampling budgets with `EnvFilter` patterns.
  Events displaced from one budget's reservoir are passed to the next matching budget.
- **Budget-based** — configure a per-second event limit for each budget; the layer
  automatically scales this to the bucket duration.
- **Low overhead** — non-matching callsites are rejected at `register_callsite` time,
  filter results are cached in a bitset, and format buffers are reused via thread-local storage.
- **Full `fmt::Layer` integration** — formatting is delegated to `tracing_subscriber::fmt::Layer`,
  so compact, pretty, JSON, timestamps, and all other formatting options work out of the box.

## Usage

```rust
use std::time::Duration;
use tracing_subscriber::{Registry, filter::EnvFilter, layer::SubscriberExt};
use tracing_log_sample::SamplingLayer;

let (layer, stats) = SamplingLayer::<Registry>::builder()
    .without_time()
    .compact()
    .bucket_duration(Duration::from_millis(50))
    .budget(EnvFilter::new("error"), 1000)   // up to 1000 error events/s
    .budget(EnvFilter::new("info"), 5000)    // up to 5000 info events/s
    .build();

let subscriber = Registry::default().with(layer);
tracing::subscriber::set_global_default(subscriber).unwrap();

// stats.received(), stats.sampled(), stats.dropped()
```

## How it works

1. Each budget has an `EnvFilter` and a reservoir sized to
   `ceil(limit_per_second * bucket_duration_secs)`.
2. On each event, the layer checks which budgets match (via a `u64` bitset),
   delegates to the inner `fmt::Layer` to format the event once, then feeds the
   formatted bytes through matching reservoirs in order.
3. If a reservoir is under capacity the event is absorbed. If full,
   Algorithm R randomly decides whether to replace an existing sample — the
   displaced event cascades to the next matching budget.
4. When the time bucket advances, all reservoirs are drained and written out.

[`tracing-subscriber`]: https://docs.rs/tracing-subscriber
[reservoir sampling]: https://en.wikipedia.org/wiki/Reservoir_sampling#Simple:_Algorithm_R
