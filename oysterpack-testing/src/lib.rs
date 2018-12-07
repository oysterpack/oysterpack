// Copyright 2018 OysterPack Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate is meant to be used as a dev dependency. Its purpose is to provides the testing support
//! to help reduce boilerplate, duplication, and provides standardization.
//!
//! The following macros are provided:
//! - [op_tests_mod](macro.op_tests_mod.html)
//!   - provides support to configure logging for tests
//!   - logs test execution time
//! - [op_test](macro.op_test.html)
//!   - used to generate test functions that leverage the `tests` module generated by [op_tests_mod!()](macro.op_tests_mod.html)
//!
//! ## Example
//! ```rust
//!
//! #[cfg(test)]
//! #[macro_use]
//! extern crate oysterpack_testing;
//!
//! #[cfg(test)]
//! op_tests_mod!();
//!
//! #[cfg(test)]
//! mod foo_test {
//!    // the macro creates a test function named 'foo'
//!    op_test!(foo, {
//!       info!("SUCCESS");
//!    });
//!
//!    #[test]
//!    fn foo_test() {
//!       // alternatively use ::run_test("test name",|| { // test code })
//!       ::run_test("foo_test", || {
//!         // by default the crate's log level is set to Debug
//!         debug!("SUCCESS")
//!       });
//!    }
//! }
//! ```
//!
//! ## Example - configuring target log levels
//! ```rust
//!
//! #[cfg(test)]
//! #[macro_use]
//! extern crate oysterpack_testing;
//!
//! #[cfg(test)]
//! op_tests_mod! {
//!     "foo" => Info,
//!     "bar" => Error
//! }
//!
//! #[cfg(test)]
//! mod foo_test {
//!    op_test!(foo, {
//!       info!("this will be logged because this crate's log level is Debug");
//!       info!(target: "foo", "foo info will be logged");
//!       info!(target: "bar", "*** bar info will not be logged ***");
//!       error!(target: "bar", "bar error will be logged");
//!    });
//!
//!    #[test]
//!    fn foo_test() {
//!       ::run_test("foo_test", || {
//!         debug!("SUCCESS")
//!       });
//!    }
//! }
//!
//! ```
//!
//! ## Notes
//! - the log, fern, and chrono crates are re-exported because they are used by the macros. Re-exporting
//!   them makes the macros self-contained.

#![deny(missing_docs, missing_debug_implementations)]
#![doc(html_root_url = "https://docs.rs/oysterpack_testing/0.1.4")]

#[allow(unused_imports)]
#[macro_use]
pub extern crate log;

pub extern crate chrono;
pub extern crate fern;

/// re-export the log macros
pub use log::{debug, error, info, log, log_enabled, trace, warn};

#[macro_use]
mod macros;

op_tests_mod! {
    "foo" => Info,
    "bar" => Error
}
