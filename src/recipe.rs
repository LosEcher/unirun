//! Recipe system v1 — per-project adaptation as data, never as code.
//!
//! Three-layer resolution (deep merge, later wins):
//!   1. built-in defaults (constants in this module)
//!   2. project recipe  `.unirun/recipe.toml` (found walking up from workdir)
//!   3. explicit CLI/MCP flags (highest priority)
//!
//! Plus a capability cache (`.unirun/capabilities.json`) with a drift check:
//! the cache is reused only while platform + arch + every recorded shell path
//! still match, otherwise it is re-probed and rewritten — killing the
//! "it worked yesterday" class of agent failures.

use crate::probe::{probe, Capabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Recipe {
    pub schema: Option<u32>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, Toolchain>,
    pub conventions: Option<Conventions>,
    pub timeouts: Option<Timeouts>,
    #[serde(default)]
    pub error_maps: BTreeMap<String, ErrorMapEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Toolchain {
    pub runner: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Conventions {
    pub line_ending: Option<String>,
    pub encoding: Option<String>,
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Timeouts {
    pub default_ms: Option<u64>,
    pub build_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorMapEntry {
    pub class: String,
    pub hint: Option<String>,
}

impl Recipe {
    /// Find and parse the nearest `.unirun/recipe.toml` walking up from `dir`.
    pub fn load_from_dir(dir: &Path) -> Option<Recipe> {
        for ancestor in dir.ancestors() {
            let candidate = ancestor.join(".unirun").join("recipe.toml");
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate).ok()?;
                return match toml::from_str(&text) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        eprintln!("unirun: recipe {} invalid: {}", candidate.display(), e);
                        None
                    }
                };
            }
        }
        None
    }

    /// Resolve a toolchain name to the first executable runner (probe-checked),
    /// with its args. Returns `(runner, args)`.
    pub fn resolve_toolchain(&self, name: &str) -> Option<(String, Vec<String>)> {
        let tc = self.toolchains.get(name)?;
        let mut candidates: Vec<String> = Vec::new();
        if let Some(r) = &tc.runner {
            candidates.push(r.clone());
        }
        candidates.extend(tc.fallbacks.iter().cloned());
        for c in candidates {
            if crate::probe::which(&c).is_some() {
                return Some((c, tc.args.clone()));
            }
        }
        None
    }

    pub fn max_output_bytes(&self) -> Option<u64> {
        self.conventions.as_ref()?.max_output_bytes
    }

    pub fn default_timeout_ms(&self) -> Option<u64> {
        self.timeouts.as_ref()?.default_ms
    }
}

/// Capability cache: read with drift check, write after a fresh probe.
pub struct CapabilityCache;

impl CapabilityCache {
    pub fn load_cached(dir: &Path) -> Option<Capabilities> {
        let path = cache_path(dir);
        let text = std::fs::read_to_string(&path).ok()?;
        let cached: Capabilities = serde_json::from_str(&text).ok()?;
        let cur = probe();
        if cached.platform == cur.platform
            && cached.arch == cur.arch
            && shells_still_valid(&cached.shells)
        {
            return Some(cached);
        }
        None
    }

    pub fn write(dir: &Path, caps: &Capabilities) -> std::io::Result<()> {
        let path = cache_path(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(caps)?)?;
        std::fs::rename(tmp, path)
    }
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join(".unirun").join("capabilities.json")
}

fn shells_still_valid(shells: &[crate::probe::ShellInfo]) -> bool {
    shells.iter().all(|s| match &s.path {
        Some(p) => Path::new(p).is_file(),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_parse_and_resolve() {
        let text = r#"
schema = 1
[toolchains.python]
runner = "python3"
fallbacks = ["python", "py"]
args = ["-u"]

[toolchains.node]
runner = "pnpm"
fallbacks = ["npm", "yarn"]

[conventions]
max_output_bytes = 65536

[timeouts]
default_ms = 30000

[error_maps]
"ModuleNotFoundError: *" = { class = "DEPENDENCY_MISSING", hint = "run `uv sync`" }
"#;
        let r: Recipe = toml::from_str(text).unwrap();
        assert_eq!(r.schema, Some(1));
        assert_eq!(r.timeouts.as_ref().unwrap().default_ms, Some(30_000));
        assert_eq!(
            r.conventions.as_ref().unwrap().max_output_bytes,
            Some(65_536)
        );
        assert!(r.error_maps.contains_key("ModuleNotFoundError: *"));
        // runner resolution probes PATH; hosts without python3/python/py
        // (e.g. some Windows CI images) correctly resolve to None — the
        // parsing assertions above are the real contract here.
        let Some((runner, args)) = r.resolve_toolchain("python") else {
            return;
        };
        assert_eq!(runner, "python3");
        assert_eq!(args, vec!["-u"]);
    }

    #[test]
    fn recipe_missing_toolchain_is_none() {
        let r = Recipe::default();
        assert!(r.resolve_toolchain("python").is_none());
    }

    #[test]
    fn cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("unirun-cache-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".unirun")).unwrap();
        let caps = probe();
        CapabilityCache::write(&dir, &caps).unwrap();
        let loaded = CapabilityCache::load_cached(&dir);
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().platform, caps.platform);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_invalidated_when_shell_missing() {
        let dir = std::env::temp_dir().join(format!("unirun-cache2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".unirun")).unwrap();
        let mut caps = probe();
        // Break one recorded path so the drift check rejects the cache.
        if let Some(s) = caps.shells.iter_mut().find(|s| s.name == "bash") {
            s.path = Some("/nonexistent/bash".into());
        }
        CapabilityCache::write(&dir, &caps).unwrap();
        assert!(CapabilityCache::load_cached(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
