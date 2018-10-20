// Copyright 2018 OysterPack Inc.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


#[macro_use]
extern crate oysterpack;
#[macro_use]
extern crate log;
extern crate simple_logging;

use oysterpack::semver;

use log::LevelFilter;

op_build_mod!();

#[test]
fn test() {
    simple_logging::log_to_stderr(LevelFilter::Info);
    let app_build = build::get();
    let version : &semver::Version = app_build.package().version();
    info!("{}-{}", build::PKG_NAME, version);
}