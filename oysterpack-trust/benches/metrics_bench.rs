/*
 * Copyright 2019 OysterPack Inc.
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

#![feature(await_macro, async_await, futures_api, arbitrary_self_types)]
#![feature(duration_float)]

#[macro_use]
extern crate criterion;

use criterion::Criterion;

use futures::{sink::SinkExt, stream::StreamExt, task::SpawnExt};
use oysterpack_log::*;
use oysterpack_trust::{
    concurrent::execution::*,
    metrics::{self, *},
};
use oysterpack_uid::ULID;
use std::time::Instant;

criterion_group!(
    benches,
    prometheus_histogram_vec_observe_bench,
    metrics_local_counter_bench
);

criterion_main!(benches);

/// take away is that spawning async futures provides too much overhead
fn metrics_local_counter_bench(c: &mut Criterion) {
    /// Local counter is designed to be non-blocking
    #[derive(Debug, Clone)]
    struct LocalCounter {
        sender: futures::channel::mpsc::Sender<CounterMessage>,
        executor: Executor,
    }

    impl LocalCounter {
        /// constructor
        /// - spawns a bacground async task to update the metric
        ///
        pub fn new(
            counter: prometheus::core::GenericLocalCounter<prometheus::core::AtomicI64>,
            executor: Executor,
        ) -> Result<Self, futures::task::SpawnError> {
            let (sender, mut receiver) = futures::channel::mpsc::channel(1);
            let mut executor = executor;
            let mut counter = counter;
            executor.spawn(
                async move {
                    while let Some(msg) = await!(receiver.next()) {
                        match msg {
                            CounterMessage::Inc => counter.inc(),
                            CounterMessage::Flush(reply) => {
                                counter.flush();
                                if let Err(_) = reply.send(()) {
                                    warn!("Failed to send Flush reply");
                                }
                            }
                            CounterMessage::Close => break,
                        }
                    }
                },
            )?;
            Ok(Self { sender, executor })
        }

        /// increment the counter
        pub fn inc(&mut self) -> Result<(), futures::task::SpawnError> {
            let mut sender = self.sender.clone();
            self.executor.spawn(
                async move {
                    if let Err(err) = await!(sender.send(CounterMessage::Inc)) {
                        warn!("Failed to send Inc message: {}", err);
                    }
                },
            )
        }

        /// increment the counter
        pub fn flush(&mut self) -> Result<(), futures::task::SpawnError> {
            let mut sender = self.sender.clone();
            let (tx, rx) = futures::channel::oneshot::channel();
            self.executor.spawn(
                async move {
                    if let Err(err) = await!(sender.send(CounterMessage::Flush(tx))) {
                        warn!("Failed to send Flush message: {}", err);
                    }
                },
            )?;
            self.executor.run(
                async {
                    await!(rx).unwrap();
                },
            );
            Ok(())
        }

        /// increment the counter
        pub fn close(&mut self) -> Result<(), futures::task::SpawnError> {
            let mut sender = self.sender.clone();
            self.executor.spawn(
                async move {
                    if let Err(err) = await!(sender.send(CounterMessage::Close)) {
                        warn!("Failed to send Close message: {}", err);
                    }
                },
            )
        }
    }

    /// Counter message
    #[derive(Debug)]
    enum CounterMessage {
        /// increment the counter
        Inc,
        /// flush the local counter to the registered counter
        Flush(futures::channel::oneshot::Sender<()>),
        /// close the local counter receiver channel, which drops the local counter
        Close,
    }

    let metric_id = MetricId::generate();
    let counter = metrics::registry()
        .register_int_counter(metric_id, ULID::generate().to_string().as_str(), None)
        .unwrap();
    let mut async_local_counter =
        LocalCounter::new(counter.local(), global_executor().clone()).unwrap();

    let mut local_counter = counter.local();
    c.bench_function("metrics_local_counter_bench - local", move |b| {
        b.iter(|| local_counter.inc())
    });

    c.bench_function("metrics_local_counter_bench - sync", move |b| {
        b.iter(|| counter.inc())
    });

    let mut async_local_counter_2 = async_local_counter.clone();
    c.bench_function("metrics_local_counter_bench - async", move |b| {
        b.iter(|| async_local_counter_2.inc())
    });

    async_local_counter.flush().unwrap();
    async_local_counter.close().unwrap();
}

fn prometheus_histogram_vec_observe_bench(c: &mut Criterion) {
    {
        let reqrep_timer = format!("OP{}", ULID::generate());
        let reqrep_service_id_label = format!("OP{}", ULID::generate());

        let registry = prometheus::Registry::new();
        let opts = prometheus::HistogramOpts::new(reqrep_timer, "reqrep timer".to_string());

        let reqrep_timer =
            prometheus::HistogramVec::new(opts, &[reqrep_service_id_label.as_str()]).unwrap();
        registry.register(Box::new(reqrep_timer.clone())).unwrap();

        c.bench_function("prometheus_histogram_vec_observe", move |b| {
            let mut reqrep_timer_local = reqrep_timer.local();
            let reqrep_timer =
                reqrep_timer_local.with_label_values(&[ULID::generate().to_string().as_str()]);
            b.iter(|| {
                let f = || {};
                let start = Instant::now();
                f();
                let duration = start.elapsed();
                reqrep_timer.observe(metrics::duration_as_secs_f64(duration));
                reqrep_timer.flush();
            })
        });
    }

    {
        let reqrep_timer = format!("OP{}", ULID::generate());
        let reqrep_service_id_label = format!("OP{}", ULID::generate());

        let registry = prometheus::Registry::new();
        let opts = prometheus::HistogramOpts::new(reqrep_timer, "reqrep timer".to_string());

        let reqrep_timer =
            prometheus::HistogramVec::new(opts, &[reqrep_service_id_label.as_str()]).unwrap();
        registry.register(Box::new(reqrep_timer.clone())).unwrap();

        c.bench_function("prometheus_histogram_vec_observe_no_flush", move |b| {
            let mut reqrep_timer_local = reqrep_timer.local();
            let reqrep_timer =
                reqrep_timer_local.with_label_values(&[ULID::generate().to_string().as_str()]);
            b.iter(|| {
                let f = || {};
                let start = Instant::now();
                f();
                let duration = start.elapsed();
                reqrep_timer.observe(metrics::duration_as_secs_f64(duration));
            })
        });
    }

    {
        let reqrep_timer = format!("OP{}", ULID::generate());
        let reqrep_service_id_label = format!("OP{}", ULID::generate());

        let registry = prometheus::Registry::new();
        let opts = prometheus::HistogramOpts::new(reqrep_timer, "reqrep timer".to_string());

        let reqrep_timer =
            prometheus::HistogramVec::new(opts, &[reqrep_service_id_label.as_str()]).unwrap();
        registry.register(Box::new(reqrep_timer.clone())).unwrap();

        c.bench_function("prometheus_histogram_vec_observe_float_secs", move |b| {
            let mut reqrep_timer_local = reqrep_timer.local();
            let reqrep_timer =
                reqrep_timer_local.with_label_values(&[ULID::generate().to_string().as_str()]);
            b.iter(|| {
                let f = || {};
                let start = Instant::now();
                f();
                let duration = start.elapsed();
                reqrep_timer.observe(metrics::duration_as_secs_f64(duration));
                reqrep_timer.flush();
            })
        });
    }

    {
        let reqrep_timer = format!("OP{}", ULID::generate());
        let reqrep_service_id_label = format!("OP{}", ULID::generate());

        let registry = prometheus::Registry::new();
        let opts = prometheus::HistogramOpts::new(reqrep_timer, "reqrep timer".to_string());

        let reqrep_timer =
            prometheus::HistogramVec::new(opts, &[reqrep_service_id_label.as_str()]).unwrap();
        registry.register(Box::new(reqrep_timer.clone())).unwrap();

        c.bench_function(
            "prometheus_histogram_vec_observe_float_secs_direct",
            move |b| {
                let mut reqrep_timer_local = reqrep_timer.local();
                let reqrep_timer =
                    reqrep_timer_local.with_label_values(&[ULID::generate().to_string().as_str()]);
                b.iter(|| {
                    let f = || {};
                    let start = Instant::now();
                    f();
                    let duration = start.elapsed();
                    reqrep_timer.observe(metrics::duration_as_secs_f64(duration));
                    reqrep_timer.flush();
                })
            },
        );
    }
}
