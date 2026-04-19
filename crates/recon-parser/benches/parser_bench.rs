use criterion::{criterion_group, criterion_main, Criterion};
use recon_core::lang::Language;
use recon_parser::extract;
use std::path::Path;

const RUST_SOURCE: &str = r#"
use std::collections::HashMap;

/// A configuration manager.
pub struct Config {
    values: HashMap<String, String>,
    defaults: HashMap<String, String>,
}

impl Config {
    pub fn new() -> Self {
        Self {
            values: HashMap::new(),
            defaults: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .or_else(|| self.defaults.get(key))
            .map(|s| s.as_str())
    }

    pub fn set(&mut self, key: String, value: String) {
        self.values.insert(key, value);
    }

    pub fn set_default(&mut self, key: String, value: String) {
        self.defaults.insert(key, value);
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().chain(self.defaults.keys()).map(|k| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

pub fn validate_config(config: &Config) -> Result<(), String> {
    if config.get("host").is_none() {
        return Err("missing host".into());
    }
    if config.get("port").is_none() {
        return Err("missing port".into());
    }
    Ok(())
}

pub fn merge_configs(base: &Config, overlay: &Config) -> Config {
    let mut result = Config::new();
    for key in base.keys() {
        if let Some(val) = base.get(key) {
            result.set(key.to_string(), val.to_string());
        }
    }
    for key in overlay.keys() {
        if let Some(val) = overlay.get(key) {
            result.set(key.to_string(), val.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut cfg = Config::new();
        cfg.set("host".into(), "localhost".into());
        cfg.set("port".into(), "8080".into());
        cfg
    }

    fn test_validate() {
        let cfg = test_config();
        assert!(validate_config(&cfg).is_ok());
    }
}
"#;

const PYTHON_SOURCE: &str = r#"
"""Configuration management module."""

from typing import Dict, Optional, Iterator
from dataclasses import dataclass, field

@dataclass
class Config:
    """A configuration manager."""
    values: Dict[str, str] = field(default_factory=dict)
    defaults: Dict[str, str] = field(default_factory=dict)

    def get(self, key: str) -> Optional[str]:
        return self.values.get(key) or self.defaults.get(key)

    def set(self, key: str, value: str) -> None:
        self.values[key] = value

    def set_default(self, key: str, value: str) -> None:
        self.defaults[key] = value

    def keys(self) -> Iterator[str]:
        yield from self.values
        yield from self.defaults

    def __len__(self) -> int:
        return len(self.values)


def validate_config(config: Config) -> None:
    if config.get("host") is None:
        raise ValueError("missing host")
    if config.get("port") is None:
        raise ValueError("missing port")


def merge_configs(base: Config, overlay: Config) -> Config:
    result = Config()
    for key in base.keys():
        val = base.get(key)
        if val is not None:
            result.set(key, val)
    for key in overlay.keys():
        val = overlay.get(key)
        if val is not None:
            result.set(key, val)
    return result
"#;

fn bench_parse_rust(c: &mut Criterion) {
    let src = RUST_SOURCE.as_bytes();
    let path = Path::new("src/config.rs");
    c.bench_function("parse_rust/100_lines", |b| {
        b.iter(|| extract::extract_symbols(src, Language::Rust, path))
    });
}

fn bench_parse_python(c: &mut Criterion) {
    let src = PYTHON_SOURCE.as_bytes();
    let path = Path::new("src/config.py");
    c.bench_function("parse_python/50_lines", |b| {
        b.iter(|| extract::extract_symbols(src, Language::Python, path))
    });
}

criterion_group!(benches, bench_parse_rust, bench_parse_python);
criterion_main!(benches);
