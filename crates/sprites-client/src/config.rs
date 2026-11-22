use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteConfig {
    pub cli_path: PathBuf,
    pub name_prefix: String,
    pub org: String,
    pub command_timeout: Duration,
}

impl Default for SpriteConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("sprite"),
            // Wave E: lane sprite names are `squadron-<run>-impl-<n>`.
            name_prefix: "squadron".to_owned(),
            org: "felix-382".to_owned(),
            command_timeout: Duration::from_secs(120),
        }
    }
}

impl SpriteConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(path) = std::env::var("SPRITE_CLI") {
            config.cli_path = PathBuf::from(path);
        }
        if let Ok(prefix) = std::env::var("SPRITE_NAME_PREFIX") {
            config.name_prefix = prefix;
        }
        if let Ok(org) = std::env::var("SPRITE_ORG") {
            config.org = org;
        }
        config
    }

    pub fn sprite_name(&self, suffix: &str) -> String {
        format!("{}-{}", self.name_prefix, suffix)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name_prefix.is_empty()
            || !self
                .name_prefix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(
                "name_prefix must contain only lowercase letters, digits and dashes".into(),
            );
        }
        if self.org.is_empty() {
            return Err("org must not be empty".into());
        }
        Ok(())
    }
}
