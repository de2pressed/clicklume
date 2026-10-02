//! Configuration module for clicklume
//! Handles loading and saving config.toml with hotkey and CPS settings

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Key to toggle autoclicking
    pub toggle: String,
    /// Key to increase CPS
    pub increase: String,
    /// Key to decrease CPS
    pub decrease: String,
    /// Key to quit the application
    pub quit: String,
    /// Default clicks per second
    pub default_cps: u32,
    /// Minimum CPS allowed
    pub min_cps: u32,
    /// Maximum CPS allowed
    pub max_cps: u32,
    /// Mouse button to click (left, right, middle)
    pub button: String,
    /// Click mode (single, double, hold)
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Whether to randomize inter-click interval
    #[serde(default)]
    pub randomize: bool,
    /// Jitter range in milliseconds (0-100)
    #[serde(default)]
    pub jitter_ms: u32,
    /// Number of click actions before stopping. Zero means unlimited.
    #[serde(default)]
    pub repeat_count: u32,
}

fn default_mode() -> String {
    "single".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            toggle: "KEY_F6".to_string(),
            increase: "KEY_F7".to_string(),
            decrease: "KEY_F8".to_string(),
            quit: "KEY_F9".to_string(),
            default_cps: 20,
            min_cps: 1,
            max_cps: 1000,
            button: "left".to_string(),
            mode: "single".to_string(),
            randomize: false,
            jitter_ms: 10,
            repeat_count: 0,
        }
    }
}

impl Config {
    /// Get the configuration directory path
    fn get_config_dir() -> PathBuf {
        Self::get_config_base().join("clicklume")
    }

    fn get_legacy_config_path() -> PathBuf {
        Self::get_config_base()
            .join("autoclick")
            .join("config.toml")
    }

    fn get_config_base() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Get the full config file path
    fn get_config_path() -> PathBuf {
        Self::get_config_dir().join("config.toml")
    }

    /// Load configuration from file or create default
    pub fn load() -> Result<Self> {
        let config_path = Self::get_config_path();

        if config_path.exists() {
            let contents =
                fs::read_to_string(&config_path).context("Failed to read config file")?;
            let config: Config =
                toml::from_str(&contents).context("Failed to parse config.toml")?;
            log::info!("Loaded configuration from {:?}", config_path);
            Ok(config)
        } else if Self::get_legacy_config_path().exists() {
            let legacy_path = Self::get_legacy_config_path();
            let contents =
                fs::read_to_string(&legacy_path).context("Failed to read legacy config file")?;
            let config: Config =
                toml::from_str(&contents).context("Failed to parse legacy config.toml")?;
            config.save()?;
            log::info!(
                "Migrated configuration from {:?} to {:?}",
                legacy_path,
                config_path
            );
            Ok(config)
        } else {
            // Create default config
            let config = Config::default();
            config.save()?;
            log::info!("Created default configuration at {:?}", config_path);
            Ok(config)
        }
    }

    /// Save configuration to file
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let config_path = Self::get_config_path();

        // Create directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).context("Failed to create config directory")?;
        }

        let contents = toml::to_string_pretty(self).context("Failed to serialize config")?;
        common::write_atomic(&config_path, contents.as_bytes())
            .context("Failed to write config file")?;
        Ok(())
    }

    /// Get CPS step based on current CPS value
    pub fn get_cps_step(cps: u32) -> u32 {
        match cps {
            1..=20 => 1,
            21..=50 => 5,
            51..=100 => 10,
            101..=500 => 25,
            501..=1000 => 50,
            _ => 1,
        }
    }

    pub fn clamp_cps(&self, cps: u32) -> u32 {
        let min = self.min_cps.clamp(1, 1000);
        let max = self.max_cps.clamp(min, 1000);
        cps.clamp(min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn smart_cps_steps_match_rate_bands() {
        assert_eq!(Config::get_cps_step(1), 1);
        assert_eq!(Config::get_cps_step(20), 1);
        assert_eq!(Config::get_cps_step(21), 5);
        assert_eq!(Config::get_cps_step(51), 10);
        assert_eq!(Config::get_cps_step(101), 25);
        assert_eq!(Config::get_cps_step(501), 50);
    }

    #[test]
    fn configured_cps_is_clamped_to_safe_bounds() {
        let config = Config {
            min_cps: 10,
            max_cps: 100,
            ..Config::default()
        };
        assert_eq!(config.clamp_cps(1), 10);
        assert_eq!(config.clamp_cps(50), 50);
        assert_eq!(config.clamp_cps(1000), 100);
    }
}
