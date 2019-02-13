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
use oysterpack_trust::metrics;
use std::{collections::HashMap, sync::Arc};

steps!(crate::TestContext => {

    given regex "01D3J3D7PA4NR9JABZWT635S6B-1" |world, matches, step| {
        register_metrics(world, matches, step);
    };

    then regex "01D3J3D7PA4NR9JABZWT635S6B-2" |world, matches, step| {
        get_metric_descs(world, matches, step);
    };

    then regex "01D3J3D7PA4NR9JABZWT635S6B-3" |world, matches, step| {
        gather_metrics(world, matches, step);
    };

    then regex "01D3J3D7PA4NR9JABZWT635S6B-4" |world, matches, step| {
        gather_metrics_using_desc_ids(world, matches, step);
    };

    then regex "01D3J3D7PA4NR9JABZWT635S6B-5" |world, matches, step| {
        gather_metrics_by_name(world, matches, step);
    };

    given regex "01D3J3DRS0CJ2YN99KAWQ19103-1" |world, matches, step| {
        given_global_registry(world, matches, step);
        register_metric(world, matches, step);
    };

    when regex "01D3J3DRS0CJ2YN99KAWQ19103-2" |world, matches, step| {
        register_duplicate_metric(world, matches, step);
    };

    then regex "01D3J3DRS0CJ2YN99KAWQ19103-3" |world, matches, step| {
        registering_duplicate_metric_fails(world, matches, step);
    };

});

fn given_global_registry(
    world: &mut crate::TestContext,
    _matches: &[String],
    _step: &gherkin::Step,
) {
    world.init();
}

fn register_metrics(world: &mut crate::TestContext, _matches: &[String], step: &gherkin::Step) {
    let mut metrics = HashMap::<metrics::MetricId, Arc<dyn prometheus::core::Collector>>::new();
    for ref tables in step.table.as_ref() {
        for row in tables.rows.iter() {
            match row[0].as_str() {
                "IntCounter" => {
                    let metric_id = metrics::MetricId::generate();
                    let counter = metrics::registry()
                        .register_int_counter(metric_id, "IntCounter", None)
                        .unwrap();
                    counter.inc();
                    metrics.insert(metric_id, Arc::new(counter));
                }
                "Counter" => {
                    let metric_id = metrics::MetricId::generate();
                    let counter = metrics::registry()
                        .register_counter(metric_id, "Counter", None)
                        .unwrap();
                    counter.inc();
                    metrics.insert(metric_id, Arc::new(counter));
                }
                _ => panic!("unsupported metric type: {}", row[0]),
            }
        }
    }
    world.metrics = Some(metrics);
}

fn register_metric(world: &mut crate::TestContext, _matches: &[String], _step: &gherkin::Step) {
    let metric_id = metrics::MetricId::generate();
    world.metric_id = Some(metric_id);
    let counter = metrics::registry()
        .register_int_counter(metric_id, "counter", None)
        .unwrap();
    counter.inc();
}

fn register_duplicate_metric(
    world: &mut crate::TestContext,
    _matches: &[String],
    _step: &gherkin::Step,
) {
    for metric_id in world.metric_id {
        world.int_counter_registration_result =
            Some(metrics::registry().register_int_counter(metric_id, "counter", None));
    }
}

fn registering_duplicate_metric_fails(
    world: &mut crate::TestContext,
    _matches: &[String],
    _step: &gherkin::Step,
) {
    for result in world.int_counter_registration_result.take() {
        match result {
            Err(err) => eprintln!("{}", err),
            Ok(_) => panic!("metric should have failed to register because it is a duplicate"),
        }
    }
}

fn get_metric_descs(world: &mut crate::TestContext, _matches: &[String], _step: &gherkin::Step) {
    assert!(world.metrics.is_some());
    let descs = metrics::registry().descs();
    for metrics in world.metrics.as_ref() {
        for metric_id in metrics.keys() {
            metrics::registry()
                .filter_descs(|desc| desc.fq_name == metric_id.name())
                .first()
                .unwrap();
            assert!(descs.iter().any(|desc| desc.fq_name == metric_id.name()));
        }
    }
}

fn gather_metrics(world: &mut crate::TestContext, _matches: &[String], _step: &gherkin::Step) {
    let metric_families = metrics::registry().gather();
    assert!(!metric_families.is_empty());
    assert!(world.metrics.is_some());
    for metrics in world.metrics.iter() {
        assert!(!metrics.is_empty());
        assert!(metric_families.len() >= metrics.len());
        for metric_id in metrics.keys() {
            let metric_name = metric_id.name();
            assert!(metric_families
                .iter()
                .any(|mf| mf.get_name() == metric_name.as_str()));
        }
    }
}

fn gather_metrics_using_desc_ids(
    world: &mut crate::TestContext,
    _matches: &[String],
    _step: &gherkin::Step,
) {
    let metric_families = metrics::registry().gather();
    assert!(!metric_families.is_empty());
    assert!(world.metrics.is_some());
    let registry = metrics::registry();
    for metrics in world.metrics.iter() {
        assert!(!metrics.is_empty());
        assert!(metric_families.len() >= metrics.len());
        for metric in metrics.values() {
            for desc in metric.desc() {
                let metric_families = registry.gather_metrics(&[desc.id]);
                assert_eq!(metric_families.len(), 1);
                assert_eq!(
                    metric_families.first().unwrap().get_name(),
                    desc.fq_name.as_str()
                );
            }
        }
    }
}

fn gather_metrics_by_name(
    world: &mut crate::TestContext,
    _matches: &[String],
    _step: &gherkin::Step,
) {
    let metric_families = metrics::registry().gather();
    assert!(!metric_families.is_empty());
    assert!(world.metrics.is_some());
    let registry = metrics::registry();
    for metrics in world.metrics.iter() {
        assert!(!metrics.is_empty());
        assert!(metric_families.len() >= metrics.len());
        for metric in metrics.values() {
            for desc in metric.desc() {
                let metric_families = registry.gather_metrics_by_name(&[desc.fq_name.as_str()]);
                assert_eq!(metric_families.len(), 1);
                assert_eq!(
                    metric_families.first().unwrap().get_name(),
                    desc.fq_name.as_str()
                );
            }
        }
    }
}
