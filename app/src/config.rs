// SPDX-License-Identifier: MIT

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

/// Whole-app configuration persisted by cosmic-config.
#[derive(Debug, Clone, CosmicConfigEntry, PartialEq)]
#[version = 1]
pub struct Config {
    /// Last globe center latitude, restored on launch.
    pub center_latitude: f32,
    /// Last globe center longitude, restored on launch.
    pub center_longitude: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            center_latitude: 18.0,
            center_longitude: -20.0,
        }
    }
}
