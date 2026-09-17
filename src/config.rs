use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub panel: PanelConfig,
    pub keys: KeysConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PanelConfig {
    pub delay_ms: u64,
    pub max_rows: u16,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            delay_ms: 1_500,
            max_rows: 8,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct KeysConfig {
    pub forward_ctrl_x: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: PathBuf,
    pub warnings: Vec<String>,
    pub environment_overrides: Vec<&'static str>,
}

impl LoadedConfig {
    pub fn print_effective(&self) -> String {
        let mut out = format!("# config file: {}\n", self.path.display());
        if self.environment_overrides.is_empty() {
            out.push_str("# environment overrides: none\n");
        } else {
            out.push_str("# environment overrides: ");
            out.push_str(&self.environment_overrides.join(", "));
            out.push('\n');
        }
        out.push_str(&toml::to_string_pretty(&self.config).expect("Config is TOML-serializable"));
        out
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> LoadedConfig {
        let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
        let mut warnings = Vec::new();
        let mut config = match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => config,
                Err(error) => {
                    warnings.push(format!(
                        "could not parse {}: {error}; using defaults",
                        path.display()
                    ));
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                warnings.push(format!(
                    "could not read {}: {error}; using defaults",
                    path.display()
                ));
                Self::default()
            }
        };
        let mut environment_overrides = Vec::new();

        apply_env(
            &mut config.panel.delay_ms,
            "CMDQ_PANEL_DELAY_MS",
            &mut warnings,
            &mut environment_overrides,
        );
        apply_env(
            &mut config.panel.max_rows,
            "CMDQ_PANEL_MAX_ROWS",
            &mut warnings,
            &mut environment_overrides,
        );
        apply_env(
            &mut config.keys.forward_ctrl_x,
            "CMDQ_FORWARD_CTRL_X",
            &mut warnings,
            &mut environment_overrides,
        );

        if config.panel.max_rows == 0 {
            warnings.push("panel.max_rows must be at least 1; using 1".into());
            config.panel.max_rows = 1;
        }

        LoadedConfig {
            config,
            path,
            warnings,
            environment_overrides,
        }
    }
}

fn apply_env<T>(
    target: &mut T,
    name: &'static str,
    warnings: &mut Vec<String>,
    overrides: &mut Vec<&'static str>,
) where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Ok(value) = std::env::var(name) else {
        return;
    };
    match value.parse() {
        Ok(parsed) => {
            *target = parsed;
            overrides.push(name);
        }
        Err(error) => warnings.push(format!("ignoring invalid {name}={value:?}: {error}")),
    }
}

pub fn default_path() -> PathBuf {
    crate::paths::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cmdq")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_uses_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = Config::load(Some(&temp.path().join("missing.toml")));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn partial_file_keeps_unspecified_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[panel]\ndelay_ms = 25\n").unwrap();
        let loaded = Config::load(Some(&path));
        assert_eq!(loaded.config.panel.delay_ms, 25);
        assert_eq!(loaded.config.panel.max_rows, 8);
        assert!(!loaded.config.keys.forward_ctrl_x);
    }

    #[test]
    fn malformed_file_warns_and_uses_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[panel\n").unwrap();
        let loaded = Config::load(Some(&path));
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
    }
}
