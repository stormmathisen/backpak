use std::{fs, io};

use anyhow::{Context, Result, anyhow};
use byte_unit::Byte;
use camino::Utf8PathBuf;
use serde_derive::Deserialize;
use tracing::*;

use crate::backend::cache;

// Big Macro demands this be a function and not a value
#[inline]
fn defcachesize() -> Byte {
    cache::DEFAULT_SIZE
}

#[derive(Debug, Deserialize)]
pub struct Configuration {
    #[serde(default = "defcachesize")]
    pub cache_size: Byte,

    #[serde(default)]
    pub skips: Vec<String>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            cache_size: cache::DEFAULT_SIZE,
            skips: vec![],
        }
    }
}

pub fn load(p: Option<Utf8PathBuf>) -> Result<Configuration> {
    let confpath: Result<Utf8PathBuf> = match p {
        Some(p) => {
            if p.as_str().is_empty() {
                debug!("Using default config per --config \"\"");
                return Ok(Configuration::default());
            } else {
                Ok(p)
            }
        }
        None => {
            let mut c: Utf8PathBuf = home::home_dir()
                .ok_or_else(|| anyhow!("Can't find home directory"))?
                .try_into()
                .context("Home directory isn't UTF-8")?;
            c.extend([".config", "backpak.toml"]);
            Ok(c)
        }
    };
    let confpath = confpath?;
    let s = match fs::read_to_string(&confpath) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Configuration::default()),
        found => found,
    }
    .with_context(|| format!("Couldn't open {confpath}"))?;
    let conf = toml::from_str(&s).with_context(|| format!("Couldn't parse {confpath}"))?;
    Ok(conf)
}
