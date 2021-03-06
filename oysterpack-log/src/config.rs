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

//! Log config

use log::Level;
use std::{collections::BTreeMap, fmt};

/// Log config
#[derive(Debug, Serialize, Deserialize)]
pub struct LogConfig {
    root_level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_levels: Option<BTreeMap<Target, Level>>,
}

impl LogConfig {
    /// Returns the root log level.
    pub fn root_level(&self) -> Level {
        self.root_level
    }

    /// Returns the configured target log levels
    pub fn target_levels(&self) -> Option<&BTreeMap<Target, Level>> {
        self.target_levels.as_ref()
    }
}

impl Default for LogConfig {
    /// Creates a default LogConfig with the root log level set to Warn and logs to stdout
    fn default() -> Self {
        LogConfig {
            root_level: Level::Warn,
            target_levels: None,
        }
    }
}

impl fmt::Display for LogConfig {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(serde_json::to_string_pretty(self).unwrap().as_str())
    }
}

/// LogConfig builder
#[derive(Debug)]
pub struct LogConfigBuilder {
    config: LogConfig,
}

impl LogConfigBuilder {
    /// Constructs a new LogConfigBuilder with the specified root log level
    pub fn new(root_level: Level) -> Self {
        let mut config: LogConfig = LogConfig::default();
        config.root_level = root_level;
        LogConfigBuilder { config }
    }

    /// Sets the log level for the specified target
    pub fn target_level(mut self, target: Target, level: Level) -> Self {
        self.config
            .target_levels
            .get_or_insert(BTreeMap::new())
            .insert(target, level);
        self
    }

    /// Builds and returns the LogConfig
    pub fn build(self) -> LogConfig {
        self.config
    }
}

/// Represents a log target
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Target(String);

impl Target {
    /// Constructor
    pub fn new(value: String) -> Self {
        Target(value)
    }

    /// Constructs a new Target by appending the specified target.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use oysterpack_log::config::Target;
    /// let foo = Target::from("foo");
    /// let foo_bar = foo.append(Target::from("bar"));
    /// assert_eq!(Target::from("foo::bar"), foo_bar);
    /// ```
    pub fn append<T>(&self, target: T) -> Target
    where
        T: Into<Target>,
    {
        Target::new(format!("{}::{}", self.0, target.into().0))
    }
}

impl<'a> From<&'a str> for Target {
    fn from(target: &'a str) -> Self {
        Target(target.to_string())
    }
}

impl AsRef<str> for Target {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use serde_json;

    #[test]
    fn root_log_level_configured() {
        crate::run_test("root_log_level_configured", || {
            let config = LogConfigBuilder::new(Level::Info).build();
            info!("{}", serde_json::to_string(&config).unwrap());
            assert_eq!(config.root_level(), Level::Info);
        });
    }

    #[test]
    fn default_log_config() {
        crate::run_test("default_log_config", || {
            let config: LogConfig = Default::default();
            info!("{}", serde_json::to_string(&config).unwrap());
            assert_eq!(config.root_level(), Level::Warn);
            assert!(config.target_levels().is_none());
        });
    }

    #[test]
    fn log_config_with_all_fields_configured() {
        crate::run_test("default_log_config", || {
            let config = LogConfigBuilder::new(Level::Info)
                .target_level(Target::from(env!("CARGO_PKG_NAME")), Level::Info)
                .target_level("a".into(), Level::Info)
                .target_level("a".into(), Level::Warn)
                .target_level("b".into(), Level::Error)
                .target_level("c".into(), Level::Debug)
                .build();
            info!("{}", serde_json::to_string_pretty(&config).unwrap());
            assert_eq!(*config.target_levels().unwrap(), {
                let mut map = BTreeMap::new();
                map.insert("a".into(), Level::Warn);
                map.insert("b".into(), Level::Error);
                map.insert("c".into(), Level::Debug);
                map.insert(Target::from(env!("CARGO_PKG_NAME")), Level::Info);
                map
            });
        });
    }

}
