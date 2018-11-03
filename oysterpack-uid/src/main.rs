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

extern crate oysterpack_uid;

use oysterpack_uid::Uid;

struct T;

fn main() {
    let ulid: Uid<T> = Uid::new();
    let mut json = String::with_capacity(128);
    json.push_str(r#"{"ulid":""#);
    json.push_str(ulid.to_string().as_str());
    json.push_str(r#"", "datetime":""#);
    json.push_str(ulid.datetime().to_string().as_str());
    json.push_str(r#"", "id":"#);
    json.push_str(ulid.id().to_string().as_str());
    json.push_str("}");
    println!("{}", json);
}
