use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/ouman-serial.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub serial_port: Option<String>,
    pub listen: String,
    pub token: Option<String>,
    pub device_id: Option<String>,
    pub smsc: String,
    pub sender_prefix: String,
    pub daily_limit: u32,
    pub alarm_hook: Option<String>,
    pub queue_max: usize,
    pub trace: bool,
    pub timeouts: Timeouts,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub receive_s: f64,
    pub reply_s: f64,
    pub poll_silence_s: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial_port: None,
            listen: "127.0.0.1:8765".into(),
            token: None,
            device_id: None,
            smsc: "+358447983500".into(),
            sender_prefix: "+35800000".into(),
            daily_limit: 100,
            alarm_hook: None,
            queue_max: 10,
            trace: false,
            timeouts: Timeouts::default(),
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            receive_s: 30.0,
            reply_s: 10.0,
            poll_silence_s: 10.0,
        }
    }
}

impl Timeouts {
    pub fn receive(&self) -> Duration {
        Duration::from_secs_f64(self.receive_s)
    }

    pub fn reply(&self) -> Duration {
        Duration::from_secs_f64(self.reply_s)
    }

    pub fn poll_silence(&self) -> Duration {
        Duration::from_secs_f64(self.poll_silence_s)
    }
}

impl Config {
    /// Reads `path`. Without a path, reads the default file if it exists.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = match path {
            Some(path) => path,
            None if Path::new(DEFAULT_PATH).exists() => Path::new(DEFAULT_PATH),
            None => return Ok(Self::default()),
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let digits = self
            .sender_prefix
            .strip_prefix('+')
            .unwrap_or(&self.sender_prefix);
        if !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() + 4 > 20 {
            bail!(
                "sender_prefix must be digits, optionally with a leading +, and at most 16 digits"
            );
        }
        let smsc = self.smsc.strip_prefix('+').unwrap_or(&self.smsc);
        if !smsc.bytes().all(|b| b.is_ascii_digit()) || smsc.len() > 20 {
            bail!("smsc must be a phone number");
        }
        if self
            .device_id
            .as_deref()
            .is_some_and(|id| id.contains(char::is_whitespace))
        {
            bail!("device_id must not contain spaces");
        }
        if self.queue_max == 0 {
            bail!("queue_max must be at least 1");
        }
        let t = &self.timeouts;
        for (name, value) in [
            ("receive_s", t.receive_s),
            ("reply_s", t.reply_s),
            ("poll_silence_s", t.poll_silence_s),
        ] {
            if !(value.is_finite() && value > 0.0) {
                bail!("timeouts.{name} must be a positive number");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_file() {
        let config: Config = toml::from_str(
            r#"
            serial_port = "/dev/ttyUSB0"
            listen = "0.0.0.0:8765"
            token = "secret"
            device_id = "TC01"
            daily_limit = 50
            alarm_hook = "logger -t ouman"
            trace = true

            [timeouts]
            receive_s = 20
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.serial_port.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(config.timeouts.receive(), Duration::from_secs(20));
        assert_eq!(config.timeouts.reply(), Duration::from_secs(10));
        assert_eq!(config.sender_prefix, "+35800000");
    }

    #[test]
    fn refuses_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<Config>("serialport = \"x\"").is_err());
        let config: Config = toml::from_str("sender_prefix = \"+35800000000000000\"").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[timeouts]\nreply_s = 0").unwrap();
        assert!(config.validate().is_err());
    }
}
