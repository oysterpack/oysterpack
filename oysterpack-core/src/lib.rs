/*
 * Copyright 2018 OysterPack Inc.
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

//! A popular technique for organizing the execution of complex processing flows is the "Chain of Responsibility" pattern,
//! as described (among many other places) in the classic "Gang of Four" design patterns book. Although
//! the fundamental API contracts required to implement this design patten are extremely simple, it is
//! useful to have a base API that facilitates using the pattern, and (more importantly) encouraging
//! composition of command implementations from multiple diverse sources.
//!
//! This implementation provides support for async commands, i.e., command futures.

// #![deny(missing_docs, missing_debug_implementations, warnings)]
#![allow(unused_imports, dead_code)]
#![deny(missing_docs, missing_debug_implementations)]
#![allow(clippy::unreadable_literal)]
#![doc(html_root_url = "https://docs.rs/oysterpack_core/0.1.0")]

#[macro_use]
extern crate oysterpack_log;
#[macro_use]
extern crate oysterpack_events;
#[macro_use]
extern crate oysterpack_errors;
#[macro_use]
extern crate oysterpack_uid;

#[macro_use]
extern crate serde;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate crossbeam_channel;

extern crate actix;

#[macro_use]
#[cfg(test)]
extern crate oysterpack_testing;
#[macro_use]
#[cfg(test)]
extern crate oysterpack_app_metadata_macros;

#[macro_use]
mod macros;

pub mod actor;
pub mod message;

#[cfg(test)]
op_tests_mod!();

#[cfg(test)]
op_build_mod!();
