//! Recipe system v2 — per-project adaptation as data, never as code.
//!
//! Layered resolution (deep merge, later layers win):
//!   1. built-in defaults (constants in this module)
//!   2. **registry recipes** named in `extends` — the user-level recipe
//!      registry lives in `UNIRUN_HOME/recipes` (default `~/.unirun/recipes`)
//!      and holds reusable named recipes (`unirun recipe add/list/show/rm`)
//!   3. project recipe `.unirun/recipe.toml` (found walking up from workdir)
//!   4. explicit CLI/MCP flags (highest priority)
//!
//! Merge granularity: `toolchains` and `error_maps` merge per key;
//! `conventions`/`timeouts` overlay per present field (a layer only overrides
//! the fields it actually sets, so a partial registry recipe can't clobber
//! project defaults).
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
    /// Names of registry recipes to layer underneath this one, in order
    /// (earlier names are lower in the stack). P2 recipe-registry feature.
    #[serde(default)]
    pub extends: Vec<String>,
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
    /// Find and parse the nearest `.unirun/recipe.toml` walking up from `dir`,
    /// then resolve the **effective** recipe: built-in defaults ← registry
    /// recipes named in `extends` ← the project recipe itself. Warnings
    /// (missing registry recipes, extends cycles) go to stderr and are
    /// skipped fail-open — a bad registry must not break normal execution.
    pub fn load_from_dir(dir: &Path) -> Option<Recipe> {
        let raw = Self::load_raw_from_dir(dir)?;
        let (effective, warnings) = effective_recipe(&raw, &mut |name| RecipeRegistry::load(name));
        for w in warnings {
            eprintln!("unirun: {}", w);
        }
        Some(effective)
    }

    /// Parse the nearest project recipe **without** resolving `extends`.
    pub fn load_raw_from_dir(dir: &Path) -> Option<Recipe> {
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

/// The user-level recipe registry: named reusable recipes under
/// `UNIRUN_HOME/recipes` (default `~/.unirun/recipes`; Windows uses
/// `%USERPROFILE%\.unirun\recipes`). Project recipes opt in via `extends`.
pub struct RecipeRegistry;

impl RecipeRegistry {
    /// `(name, path)` for every `*.toml` in the registry, sorted by name.
    pub fn list() -> Vec<(String, PathBuf)> {
        let dir = registry_dir();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        out.push((stem.to_string(), path));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Parse a named registry recipe (raw, without resolving its own extends).
    pub fn load(name: &str) -> Option<Recipe> {
        if !valid_name(name) {
            return None;
        }
        let path = registry_dir().join(format!("{}.toml", name));
        let text = std::fs::read_to_string(&path).ok()?;
        match toml::from_str(&text) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("unirun: registry recipe {} invalid: {}", name, e);
                None
            }
        }
    }

    /// Copy a recipe file into the registry under `name`, validating both the
    /// name and that the file parses as a recipe first.
    pub fn add(name: &str, from: &Path) -> Result<PathBuf, String> {
        if !valid_name(name) {
            return Err(format!(
                "invalid recipe name `{}`: use letters, digits, `-` or `_`",
                name
            ));
        }
        let text = std::fs::read_to_string(from)
            .map_err(|e| format!("cannot read `{}`: {}", from.display(), e))?;
        toml::from_str::<Recipe>(&text)
            .map_err(|e| format!("invalid recipe `{}`: {}", from.display(), e))?;
        let dir = registry_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create registry dir `{}`: {}", dir.display(), e))?;
        let dest = dir.join(format!("{}.toml", name));
        std::fs::write(&dest, text)
            .map_err(|e| format!("cannot write `{}`: {}", dest.display(), e))?;
        Ok(dest)
    }

    /// Remove a named registry recipe.
    pub fn remove(name: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("invalid recipe name `{}`", name));
        }
        let path = registry_dir().join(format!("{}.toml", name));
        if path.is_file() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("cannot remove `{}`: {}", path.display(), e))
        } else {
            Err(format!("no registry recipe named `{}`", name))
        }
    }

    /// Validate every registry recipe: parseable, and `extends` resolves
    /// without cycles. Returns `Ok(names)` or `Err(errors)`.
    pub fn check() -> Result<Vec<String>, Vec<String>> {
        let mut errors = Vec::new();
        let mut ok = Vec::new();
        for (name, path) in Self::list() {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("{}: unreadable: {}", name, e));
                    continue;
                }
            };
            let raw: Recipe = match toml::from_str(&text) {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("{}: invalid: {}", name, e));
                    continue;
                }
            };
            let (_, warnings) = effective_recipe(&raw, &mut |n| Self::load(n));
            let cycles: Vec<String> = warnings
                .iter()
                .filter(|w| w.contains("cycle"))
                .cloned()
                .collect();
            if !cycles.is_empty() {
                errors.extend(cycles);
            } else {
                ok.push(name);
            }
        }
        if errors.is_empty() {
            Ok(ok)
        } else {
            Err(errors)
        }
    }
}

/// Directory holding the registry recipes (test override: `UNIRUN_HOME`).
pub fn registry_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("UNIRUN_HOME") {
        return PathBuf::from(h).join("recipes");
    }
    home_dir().join(".unirun").join("recipes")
}

fn home_dir() -> PathBuf {
    let from_env = || {
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOMEDRIVE")
                        .zip(std::env::var_os("HOMEPATH"))
                        .map(|(d, p)| PathBuf::from(d).join(p))
                })
        } else {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    };
    from_env().unwrap_or_else(|| PathBuf::from("."))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Overlay `layer` on top of `base` in place (deep merge, later wins).
/// `toolchains`/`error_maps` merge per key; `conventions`/`timeouts` overlay
/// only the fields the layer actually sets.
fn overlay(base: &mut Recipe, layer: Recipe) {
    for (k, v) in layer.toolchains {
        base.toolchains.insert(k, v);
    }
    if let Some(c) = layer.conventions {
        let b = base.conventions.get_or_insert_with(Conventions::default);
        if c.line_ending.is_some() {
            b.line_ending = c.line_ending;
        }
        if c.encoding.is_some() {
            b.encoding = c.encoding;
        }
        if c.max_output_bytes.is_some() {
            b.max_output_bytes = c.max_output_bytes;
        }
    }
    if let Some(t) = layer.timeouts {
        let b = base.timeouts.get_or_insert_with(Timeouts::default);
        if t.default_ms.is_some() {
            b.default_ms = t.default_ms;
        }
        if t.build_ms.is_some() {
            b.build_ms = t.build_ms;
        }
    }
    for (k, v) in layer.error_maps {
        base.error_maps.insert(k, v);
    }
}

/// Resolve `project` to its effective recipe by layering registry recipes
/// named in `extends` (recursively, cycle-guarded) under it. Returns the
/// merged recipe plus warnings (missing registry recipes, cycles).
pub fn effective_recipe(
    project: &Recipe,
    load_registry: &mut dyn FnMut(&str) -> Option<Recipe>,
) -> (Recipe, Vec<String>) {
    let mut out = Recipe::default();
    let mut stack: Vec<String> = Vec::new();
    let mut warnings = Vec::new();
    resolve_extends(project, &mut out, &mut stack, load_registry, &mut warnings);
    overlay(&mut out, project.clone());
    (out, warnings)
}

fn resolve_extends(
    recipe: &Recipe,
    out: &mut Recipe,
    stack: &mut Vec<String>,
    load_registry: &mut dyn FnMut(&str) -> Option<Recipe>,
    warnings: &mut Vec<String>,
) {
    for name in &recipe.extends {
        if stack.iter().any(|s| s == name) {
            warnings.push(format!(
                "recipe extends cycle detected at `{}` (chain: {})",
                name,
                stack.join(" -> ")
            ));
            continue;
        }
        let Some(layer) = load_registry(name) else {
            warnings.push(format!(
                "extends references missing registry recipe `{}`",
                name
            ));
            continue;
        };
        stack.push(name.clone());
        resolve_extends(&layer, out, stack, load_registry, warnings);
        overlay(out, layer);
        stack.pop();
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

    fn recipe(text: &str) -> Recipe {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn overlay_merges_toolchains_and_error_maps_by_key() {
        let base = recipe(
            r#"
[toolchains.python]
runner = "python3"
args = ["-u"]
[toolchains.node]
runner = "npm"
[error_maps]
"a: *" = { class = "NOT_FOUND", hint = "base hint" }
"#,
        );
        let layer = recipe(
            r#"
[toolchains.python]
runner = "uv"
args = ["run"]
[toolchains.go]
runner = "go"
[error_maps]
"b: *" = { class = "NETWORK", hint = "layer hint" }
"#,
        );
        let mut out = base.clone();
        overlay(&mut out, layer);
        // python overridden by the later layer, node/go preserved.
        assert_eq!(out.toolchains["python"].runner.as_deref(), Some("uv"));
        assert_eq!(out.toolchains["node"].runner.as_deref(), Some("npm"));
        assert_eq!(out.toolchains["go"].runner.as_deref(), Some("go"));
        assert!(out.error_maps.contains_key("a: *"));
        assert!(out.error_maps.contains_key("b: *"));
    }

    #[test]
    fn overlay_conventions_only_override_present_fields() {
        let base = recipe("[conventions]\nmax_output_bytes = 65536\nline_ending = \"lf\"\n");
        let layer = recipe("[conventions]\nmax_output_bytes = 131072\n");
        let mut out = base;
        overlay(&mut out, layer);
        let c = out.conventions.unwrap();
        assert_eq!(c.max_output_bytes, Some(131_072));
        assert_eq!(
            c.line_ending.as_deref(),
            Some("lf"),
            "unset field must survive"
        );
        assert_eq!(c.encoding, None);
    }

    #[test]
    fn effective_recipe_layers_registry_under_project() {
        let project = recipe(
            r#"
extends = ["python-base", "strict"]
[timeouts]
default_ms = 9000
[error_maps]
"proj: *" = { class = "PERMISSION", hint = "project hint" }
"#,
        );
        let mut fake = std::collections::HashMap::new();
        fake.insert(
            "python-base".to_string(),
            recipe(
                r#"
[toolchains.python]
runner = "python3"
args = ["-u"]
[timeouts]
default_ms = 30000
"#,
            ),
        );
        fake.insert(
            "strict".to_string(),
            recipe(
                r#"
extends = ["python-base"]
[conventions]
max_output_bytes = 262144
[toolchains.python]
runner = "uv"
"#,
            ),
        );
        let (eff, warnings) = effective_recipe(&project, &mut |name| fake.get(name).cloned());
        assert!(warnings.is_empty(), "{:?}", warnings);
        // strict's extends re-layers python-base, then project wins on timeout.
        assert_eq!(eff.timeouts.as_ref().unwrap().default_ms, Some(9_000));
        assert_eq!(
            eff.conventions.as_ref().unwrap().max_output_bytes,
            Some(262_144)
        );
        // Toolchains merge per key: strict's `python` replaces python-base's
        // entry wholesale (runner overridden, its args do not survive).
        assert_eq!(eff.toolchains["python"].runner.as_deref(), Some("uv"));
        assert!(eff.toolchains["python"].args.is_empty());
        assert_eq!(
            eff.error_maps["proj: *"].hint.as_deref(),
            Some("project hint")
        );
    }

    #[test]
    fn effective_recipe_reports_missing_and_cycles() {
        let project = recipe("extends = [\"missing-reg\", \"a\"]\n");
        let mut fake = std::collections::HashMap::new();
        fake.insert("a".to_string(), recipe("extends = [\"b\"]\n"));
        fake.insert("b".to_string(), recipe("extends = [\"a\"]\n")); // cycle a->b->a
        let (eff, warnings) = effective_recipe(&project, &mut |name| fake.get(name).cloned());
        assert!(eff.toolchains.is_empty());
        let joined = warnings.join(" | ");
        assert!(
            joined.contains("missing registry recipe `missing-reg`"),
            "{}",
            joined
        );
        assert!(joined.contains("cycle"), "{}", joined);
    }

    // Registry tests mutate UNIRUN_HOME; serialize them so parallel threads
    // can't observe each other's environment.
    static REG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_registry(f: impl FnOnce(&std::path::Path)) {
        let _guard = REG_LOCK.lock().unwrap_or_else(|e| e.into_inner()); // a panicking test must not wedge the others
        let dir = std::env::temp_dir().join(format!("unirun-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("UNIRUN_HOME");
        std::env::set_var("UNIRUN_HOME", &dir);
        f(&dir);
        match prev {
            Some(v) => std::env::set_var("UNIRUN_HOME", v),
            None => std::env::remove_var("UNIRUN_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_add_list_load_remove_roundtrip() {
        with_temp_registry(|_| {
            let src = std::env::temp_dir().join(format!("unirun-reg-src-{}", std::process::id()));
            std::fs::write(
                &src,
                "[toolchains.python]\nrunner = \"python3\"\n[toolchains.node]\nrunner = \"pnpm\"\n",
            )
            .unwrap();
            let dest = RecipeRegistry::add("py-node", &src).expect("add");
            assert!(dest.is_file());
            let listed = RecipeRegistry::list();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].0, "py-node");
            let loaded = RecipeRegistry::load("py-node").expect("load");
            assert_eq!(
                loaded.toolchains["python"].runner.as_deref(),
                Some("python3")
            );
            RecipeRegistry::remove("py-node").expect("remove");
            assert!(RecipeRegistry::list().is_empty());
            let _ = std::fs::remove_file(&src);
        });
    }

    #[test]
    fn registry_rejects_bad_names_and_bad_files() {
        with_temp_registry(|_| {
            let src = std::env::temp_dir().join(format!("unirun-reg-bad-{}", std::process::id()));
            std::fs::write(&src, "not a recipe [").unwrap();
            assert!(RecipeRegistry::add("../escape", &src).is_err());
            assert!(RecipeRegistry::add("ok-name", &src).is_err()); // invalid TOML
            let _ = std::fs::remove_file(&src);
        });
    }

    #[test]
    fn registry_check_reports_cycles() {
        with_temp_registry(|_| {
            let a = registry_dir().join("a.toml");
            std::fs::create_dir_all(registry_dir()).unwrap();
            std::fs::write(&a, "extends = [\"a\"]\n").unwrap();
            let result = RecipeRegistry::check();
            assert!(result.is_err());
            let joined = result.unwrap_err().join(" | ");
            assert!(joined.contains("cycle"), "{}", joined);
            std::fs::remove_file(&a).unwrap();
        });
    }
}
