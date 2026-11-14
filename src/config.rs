//! Bootstrap configuration (TOML file + CLI overrides).
//!
//! Only things needed before the database is open live here. A missing or
//! invalid file never stops the service: defaults are used and a warning is
//! logged.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = "/etc/hwstats/config.toml";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9393";
pub const DEFAULT_DB: &str = "/var/lib/hwstats/hwstats.db";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub listen: String,
    pub database: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub allowed_hosts: Vec<String>,
    pub log_level: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.into(),
            database: PathBuf::from(DEFAULT_DB),
            backup_dir: None,
            allowed_hosts: Vec::new(),
            log_level: "info".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub database: PathBuf,
    pub state_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub allowed_hosts: Vec<String>,
    pub log_level: String,
    /// Problems found while loading (reported after logging is set up).
    pub warnings: Vec<String>,
}

pub struct Overrides {
    pub listen: Option<String>,
    pub database: Option<PathBuf>,
    pub backup_dir: Option<PathBuf>,
}

impl Config {
    pub fn load(path: Option<&Path>, ov: Overrides) -> Config {
        let mut warnings = Vec::new();
        let explicit = path.is_some();
        let path = path.unwrap_or(Path::new(DEFAULT_CONFIG));
        let file = match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<FileConfig>(&s) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!(
                        "config {} is invalid ({e}); using defaults",
                        path.display()
                    ));
                    FileConfig::default()
                }
            },
            Err(e) => {
                if explicit || e.kind() != std::io::ErrorKind::NotFound {
                    warnings.push(format!(
                        "cannot read config {} ({e}); using defaults",
                        path.display()
                    ));
                }
                FileConfig::default()
            }
        };

        let listen_s = ov.listen.unwrap_or(file.listen);
        let listen = match listen_s.parse::<SocketAddr>() {
            Ok(a) => a,
            Err(e) => {
                warnings.push(format!(
                    "invalid listen address '{listen_s}' ({e}); using {DEFAULT_LISTEN}"
                ));
                DEFAULT_LISTEN.parse().expect("valid default")
            }
        };

        let database = ov.database.unwrap_or(file.database);
        let state_dir = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let backup_dir = ov
            .backup_dir
            .or(file.backup_dir)
            .unwrap_or_else(|| state_dir.join("backups"));

        Config {
            listen,
            database,
            state_dir,
            backup_dir,
            allowed_hosts: file.allowed_hosts,
            log_level: file.log_level,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_ov() -> Overrides {
        Overrides {
            listen: None,
            database: None,
            backup_dir: None,
        }
    }

    #[test]
    fn invalid_file_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "listen = 5\nnope = true").unwrap();
        let c = Config::load(Some(&p), no_ov());
        assert_eq!(c.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn overrides_and_derived_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "listen = \"127.0.0.1:1234\"\n").unwrap();
        let c = Config::load(
            Some(&p),
            Overrides {
                listen: None,
                database: Some(PathBuf::from("/tmp/x/h.db")),
                backup_dir: None,
            },
        );
        assert_eq!(c.listen.port(), 1234);
        assert_eq!(c.state_dir, PathBuf::from("/tmp/x"));
        assert_eq!(c.backup_dir, PathBuf::from("/tmp/x/backups"));
        assert!(c.warnings.is_empty());
    }
}
