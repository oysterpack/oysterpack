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

use cucumber_rust::*;

use float_cmp::ApproxEq;
use futures::{channel::oneshot, prelude::*, task::SpawnExt};
use oysterpack_trust::metrics::TimerBuckets;
use oysterpack_trust::{
    concurrent::{
        execution::{self, *},
        messaging::reqrep::{self, *},
    },
    metrics,
};
use prometheus::Encoder;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::Duration,
};

steps!(World => {
    // Feature: [01D4ZS9FX0GZZRG9RF072XGBQD] All ReqRep related metrics can be gathered via reqrep::gather_metrics()

    // Scenario: [01D4ZSJ5XPDVKG33NXDE6TP6QX] send requests and then gather metrics
    given regex "01D4ZSJ5XPDVKG33NXDE6TP6QX" | world, _matches, _step | {
        world.client = Some(counter_service());
    };

    when regex "01D4ZSJ5XPDVKG33NXDE6TP6QX" | world, _matches, _step | {
        world.send_requests(10, CounterRequest::Inc);
    };

    then regex "01D4ZSJ5XPDVKG33NXDE6TP6QX" | world, _matches, _step | {
        world.client.iter().for_each(|client| {
            let reqrep_id = client.id();

            // wait until all requests have been sent
            while request_send_count(reqrep_id) < 10 {
                thread::yield_now();
            }
            thread::yield_now();

            // gather metrics
            let reqrep_metrics = reqrep::gather_metrics();
            println!("{:#?}", reqrep_metrics);

            // check that all expected metrics have been gathered
            let metric_ids = vec![REQREP_PROCESS_TIMER_METRIC_ID, REQREP_SEND_COUNTER_METRIC_ID, SERVICE_INSTANCE_COUNT_METRIC_ID];
            metric_ids.iter().for_each(|meric_id| {
                let metric_name = REQREP_PROCESS_TIMER_METRIC_ID.name();
                let metric_name = metric_name.as_str();
                assert!(reqrep_metrics.iter().any(|mf| mf.get_name() == metric_name));
            });
        })
    };

    // Feature: [01D4ZS3J72KG380GFW4GMQKCFH] Message processing timer metrics are collected

    // Scenario: [01D5028W0STBFHDAPWA79B4TGG] Processor sleeps for 10 ms
    given regex "01D5028W0STBFHDAPWA79B4TGG" | world, _matches, _step | {
        world.client = Some(counter_service());
        let buckets = metrics::TimerBuckets::from(vec![
            Duration::from_millis(5),
            Duration::from_millis(10),
            Duration::from_millis(15),
            Duration::from_millis(20),
        ]);
        world.client = Some(counter_service_with_timer_buckets(buckets));
    };

    when regex "01D5028W0STBFHDAPWA79B4TGG" | world, _matches, _step | {
        const REQ_COUNT: usize = 5;
        world.send_requests(REQ_COUNT, CounterRequest::SleepAndInc(Duration::from_millis(10)));

        // wait until all requests have been sent
        'outer: loop {
            for client in world.client.as_ref() {
                if request_send_count(client.id()) == REQ_COUNT as u64 {
                    break 'outer;
                }
                println!("request_send_count = {}", request_send_count((client.id())));
                thread::sleep(Duration::from_millis(10));
            }
        }
        thread::sleep(Duration::from_millis(20));
    };

    then regex "01D5028W0STBFHDAPWA79B4TGG" | world, _matches, _step | {
        let histogram_timer = world.histogram_timer();
        println!("{:#?}", histogram_timer);
        assert_eq!(histogram_timer.get_sample_count(), 5);
        assert!(histogram_timer.get_sample_sum() > 0.050 && histogram_timer.get_sample_sum() < 0.052);
        let bucket = histogram_timer.get_bucket().iter().find(|bucket| bucket.get_cumulative_count() == 5).unwrap();
        println!("{:#?}", bucket);
        assert!(bucket.get_upper_bound().approx_eq(&0.015, std::f64::EPSILON, 2));
    };

    // Feature: [01D52CH5BJQM4D903VN1MJ10CC] The number of requests sent per ReqRepId is tracked

    // Scenario: [01D52CJPG0THNTQR04ZVTDJ3WR] Send 100 requests from multiple tasks
    then regex "01D52CJPG0THNTQR04ZVTDJ3WR" | world, _matches, _step | {
        let client = counter_service();
        let reqrep_id = client.id();
        world.client = Some(client);
        world.send_requests(100, CounterRequest::Inc);
        while reqrep::request_send_count(reqrep_id) < 100 {
            thread::yield_now();
        }
    };

    // Feature: [01D4ZHRS7RV42RXN1R83Q8QDPA] The number of running ReqRep service backend instances will be tracked

    // Scenario: [01D4ZJJJCHMHAK12MGEY5EF6VF] start up 10 instances of a ReqRep service using the same ReqRepId
    then regex "01D4ZJJJCHMHAK12MGEY5EF6VF-1" | world, _matches, _step | {
        let reqrep_id = ReqRepId::generate();
        let clients: Vec<_> = (0..10).map(|_| counter_service_with_reqrep_id(reqrep_id)).collect();
        world.clients = Some(clients);
        while reqrep::service_instance_count(reqrep_id) < 10 {
            thread::yield_now();
        }
    };

    when regex "01D4ZJJJCHMHAK12MGEY5EF6VF-2" | world, _matches, _step | {
        let mut clients = world.clients.take().unwrap();
        for _ in 0..3 {
            clients.pop();
        }
        world.clients = Some(clients);
    };

    then regex "01D4ZJJJCHMHAK12MGEY5EF6VF-3" | world, _matches, _step | {
        for clients in &world.clients {
            let reqrep_id = clients.first().unwrap().id();
            assert_eq!(reqrep::service_instance_count(reqrep_id), 7);
        }
    };


});

#[derive(Debug, Default)]
struct Counter {
    count: Arc<RwLock<usize>>,
}

impl Processor<CounterRequest, usize> for Counter {
    fn process(&mut self, req: CounterRequest) -> FutureReply<usize> {
        let count = self.count.clone();
        async move {
            match req {
                CounterRequest::Inc => {
                    let mut count = count.write().unwrap();
                    *count += 1;
                    *count
                }
                CounterRequest::Get => {
                    let count = count.read().unwrap();
                    *count
                }
                CounterRequest::Panic => panic!("BOOM !!!"),
                CounterRequest::SleepAndInc(sleep) => {
                    //println!("sleeping for {:?} ...", sleep);
                    thread::sleep(sleep);
                    let mut count = count.write().unwrap();
                    *count += 1;
                    *count
                }
            }
        }
            .boxed()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CounterRequest {
    Inc,
    Get,
    Panic,
    SleepAndInc(Duration),
}

fn counter_service() -> ReqRep<CounterRequest, usize> {
    let buckets = TimerBuckets::from(vec![
        Duration::from_nanos(100),
        Duration::from_nanos(200),
        Duration::from_nanos(300),
    ]);
    ReqRepConfig::new(ReqRepId::generate(), buckets)
        .start_service(Counter::default(), global_executor())
        .unwrap()
}

fn counter_service_with_reqrep_id(reqrep_id: ReqRepId) -> ReqRep<CounterRequest, usize> {
    let buckets = TimerBuckets::from(vec![
        Duration::from_nanos(100),
        Duration::from_nanos(200),
        Duration::from_nanos(300),
    ]);
    ReqRepConfig::new(reqrep_id, buckets)
        .start_service(Counter::default(), global_executor())
        .unwrap()
}

fn counter_service_with_timer_buckets(buckets: TimerBuckets) -> ReqRep<CounterRequest, usize> {
    ReqRepConfig::new(ReqRepId::generate(), buckets)
        .start_service(Counter::default(), global_executor())
        .unwrap()
}

fn counter_service_with_channel_size(chan_size: usize) -> ReqRep<CounterRequest, usize> {
    let buckets = TimerBuckets::from(vec![
        Duration::from_nanos(100),
        Duration::from_nanos(200),
        Duration::from_nanos(300),
    ]);
    ReqRepConfig::new(ReqRepId::generate(), buckets)
        .set_chan_buf_size(chan_size)
        .start_service(Counter::default(), global_executor())
        .unwrap()
}

#[derive(Default)]
pub struct World {
    client: Option<ReqRep<CounterRequest, usize>>,
    clients: Option<Vec<ReqRep<CounterRequest, usize>>>,
}

impl World {
    fn send_requests(&mut self, req_count: usize, request: CounterRequest) {
        for client in self.client.as_ref() {
            let mut executor = global_executor();
            let mut client = client.clone();
            executor
                .spawn(
                    async move {
                        let mut sent_count = 0;
                        for _ in 0..req_count {
                            await!(client.send(request)).unwrap();
                            sent_count += 1;
                            println!("sent_count = {}", sent_count);
                        }
                    },
                )
                .unwrap();
        }
    }

    /// returns the histogram timer metric corresponding to the ReqRepId for the current world.client
    fn histogram_timer(&self) -> prometheus::proto::Histogram {
        let reqrep_id = self.client.as_ref().iter().next().unwrap().id();
        let reqrep_id = reqrep_id.to_string();
        let reqrep_id = reqrep_id.as_str();
        let histogram: Vec<_> = metrics::registry()
            .gather_for_metric_ids(&[REQREP_PROCESS_TIMER_METRIC_ID])
            .iter()
            .filter_map(|mf| {
                let metric = &mf.get_metric().iter().next().unwrap();
                if metric
                    .get_label()
                    .iter()
                    .any(|label_pair| label_pair.get_value() == reqrep_id)
                {
                    Some(metric.get_histogram().clone())
                } else {
                    None
                }
            })
            .collect();
        histogram.first().unwrap().clone()
    }
}
