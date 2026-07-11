//! Configuration file loading for ia-get.

use crate::{IaGetError, Result};
use std::fs;
use std::path::PathBuf;

/// Runtime settings loaded from `ia-get.ini`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Maximum download speed in KB/s. `-1` means unlimited.
    pub max_bandwidth_kbps: i64,
    /// Whether to download multiple files in parallel.
    pub multithreading: bool,
    /// Number of concurrent downloads when multithreading is enabled.
    pub thread_count: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bandwidth_kbps: -1,
            multithreading: false,
            thread_count: 4,
        }
    }
}

impl Config {
    /// Load configuration from the first available `ia-get.ini`, or use defaults.
    pub fn load() -> Self {
        match Self::load_from_disk() {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Warning: could not load ia-get.ini ({e}), using defaults");
                Self::default()
            }
        }
    }

    fn load_from_disk() -> Result<Self> {
        let Some(path) = locate_config_file() else {
            return Ok(Self::default());
        };

        let content = fs::read_to_string(&path)?;
        Self::parse(&content)
    }

    fn parse(content: &str) -> Result<Self> {
        let mut config = Self::default();

        for line in content.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }

            let Some((key, value)) = line
                .split_once('=')
                .or_else(|| line.split_once(':'))
            else {
                continue;
            };

            match key.trim().to_ascii_lowercase().as_str() {
                "maxbandwidth" | "max_bandwidth" => {
                    config.max_bandwidth_kbps = value.trim().parse::<i64>().map_err(|_| {
                        IaGetError::Config(format!("Invalid maxbandwidth value: {value}"))
                    })?;
                }
                "multithreading" | "multi_threading" => {
                    config.multithreading = parse_bool(value.trim())?;
                }
                "threads" | "thread_count" => {
                    let threads = value.trim().parse::<u32>().map_err(|_| {
                        IaGetError::Config(format!("Invalid threads value: {value}"))
                    })?;
                    if threads == 0 {
                        return Err(IaGetError::Config(
                            "threads must be at least 1".to_string(),
                        ));
                    }
                    config.thread_count = threads;
                }
                _ => {}
            }
        }

        Ok(config)
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(IaGetError::Config(format!("Invalid boolean value: {value}"))),
    }
}

/// Search for `ia-get.ini` in the current directory and user config directory.
pub fn locate_config_file() -> Option<PathBuf> {
    let local = PathBuf::from("ia-get.ini");
    if local.is_file() {
        return Some(local);
    }

    user_config_path().filter(|path| path.is_file())
}

fn user_config_path() -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return Some(PathBuf::from(appdata).join("ia-get").join("ia-get.ini"));
    }

    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("ia-get")
                .join("ia-get.ini"),
        );
    }

    None
}

/// Returns the maximum download rate in bytes per second, or `None` if unlimited.
pub fn max_bytes_per_second(max_bandwidth_kbps: i64) -> Option<u64> {
    if max_bandwidth_kbps < 0 {
        None
    } else {
        Some(max_bandwidth_kbps as u64 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_values() {
        let config = Config::parse(
            "# comment\nmaxbandwidth = 4000\nmultithreading = true\nthreads = 8\n",
        )
        .unwrap();

        assert_eq!(config.max_bandwidth_kbps, 4000);
        assert!(config.multithreading);
        assert_eq!(config.thread_count, 8);
    }

    #[test]
    fn defaults_when_empty() {
        let config = Config::parse("").unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn max_bytes_per_second_conversion() {
        assert_eq!(max_bytes_per_second(-1), None);
        assert_eq!(max_bytes_per_second(4000), Some(4_096_000));
    }
}
