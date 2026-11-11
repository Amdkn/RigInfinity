//! OpenRig context packs — the native port of the daemon's context-pack
//! loading (D-N051 F8, pas 4, D4).
//!
//! Upstream (`packages/daemon/src/domain/context-packs/`, PL-014): a
//! context_pack is a directory containing `manifest.yaml` + included
//! markdown / yaml / txt files. Operator-authored, library-discoverable,
//! reviewable, sendable. Storage is filesystem-canonical at
//! `<context.root>/<ref>/` (default `$OPENRIG_HOME/context`) +
//! workspace-local `.openrig/context-packs/<name>/`. NO new SQLite tables;
//! the library cache is in-memory at the daemon scope. The taxonomy is ONE
//! definition site: `world | lore | skills | mission`
//! (`context-pack-types.ts`, OPR.0.5.6.10).
//!
//! The native port loads packs from disk with the same taxonomy, the same
//! no-SQLite rule, and the same path-containment rule (a listed file may
//! never escape its pack directory).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// The one taxonomy definition site (upstream `ATOM_TAXONOMIES`).
pub const ATOM_TAXONOMIES: &[&str] = &["world", "lore", "skills", "mission"];

/// The teaching the daemon parser and the CLI validator share.
pub const TAXONOMY_TEACHING: &str = "Legal values: world | lore | skills | mission. \
    world = where you are (the environment: entities, rules, affordances); \
    lore = what has been learned here (position knowledge earned in place); \
    skills = what you know how to do (procedural capability); \
    mission = what you are doing now (the current work and its intent). \
    Add one line to manifest.yaml, e.g. `taxonomy: skills`.";

/// One file listed in a pack manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative path within the pack directory.
    pub path: String,
    /// Free-form operator-defined role (e.g. "prd", "proof-packet").
    pub role: String,
    /// One-line description rendered in the review pane and bundle frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A context-pack manifest (`manifest.yaml`), as parsed by serde_yaml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPackManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub taxonomy: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
}

/// A loaded context pack: its manifest plus the contents of its files.
#[derive(Debug, Clone)]
pub struct ContextPack {
    pub manifest: ContextPackManifest,
    /// Directory the pack was loaded from.
    pub dir: PathBuf,
    /// File contents, keyed by the manifest-relative path.
    pub contents: HashMap<String, String>,
}

impl ContextPack {
    /// The pack's name: the manifest name, or the directory name.
    pub fn name(&self) -> String {
        if let Some(name) = &self.manifest.name {
            if !name.is_empty() {
                return name.clone();
            }
        }
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Validate a parsed manifest: taxonomy in the four legal values; every
/// listed file has a path and a role; paths stay inside the pack directory
/// (no `..` escape, no absolute path). Returns the error strings.
pub fn validate_manifest(manifest: &ContextPackManifest) -> Vec<String> {
    let mut errors = Vec::new();
    if !ATOM_TAXONOMIES.contains(&manifest.taxonomy.as_str()) {
        errors.push(format!(
            "manifest taxonomy {:?} is not one of {} — {}",
            manifest.taxonomy,
            ATOM_TAXONOMIES.join(" | "),
            TAXONOMY_TEACHING
        ));
    }
    for file in &manifest.files {
        if file.path.is_empty() {
            errors.push("manifest file without a path".to_string());
            continue;
        }
        if file.path.starts_with('/') || file.path.split('/').any(|part| part == "..") {
            errors.push(format!(
                "manifest file path {:?} escapes the pack directory",
                file.path
            ));
        }
        if file.role.is_empty() {
            errors.push(format!("manifest file {} without a role", file.path));
        }
    }
    errors
}

/// Load one pack from a directory: read + parse `manifest.yaml`, validate
/// it, then read every listed file. A missing file or an invalid manifest
/// is an error naming the pack and the cause — never a silent skip.
pub fn load_pack(dir: &Path) -> Result<ContextPack, BoxError> {
    let manifest_path = dir.join("manifest.yaml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: ContextPackManifest = serde_yaml::from_str(&text)
        .map_err(|e| format!("cannot parse {}: {e}", manifest_path.display()))?;
    let errors = validate_manifest(&manifest);
    if !errors.is_empty() {
        return Err(format!("invalid manifest {}: {}", manifest_path.display(), errors.join("; ")).into());
    }
    let mut contents = HashMap::new();
    for file in &manifest.files {
        let path = dir.join(&file.path);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read pack file {}: {e}", path.display()))?;
        contents.insert(file.path.clone(), content);
    }
    Ok(ContextPack { manifest, dir: dir.to_path_buf(), contents })
}

/// The default context-pack root: `$OPENRIG_HOME/context`.
pub fn resolve_context_pack_root(openrig_home: &Path) -> PathBuf {
    openrig_home.join("context")
}

/// The pack library: every pack under a root, held in memory (the
/// upstream cache is in-memory at the daemon scope — no SQLite tables).
#[derive(Debug, Default)]
pub struct ContextPackLibrary {
    packs: HashMap<String, ContextPack>,
}

impl ContextPackLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a root directory and load every sub-directory that holds a
    /// `manifest.yaml`. Directories without one are ignored (they are not
    /// packs). A pack that fails to load is an error — a broken pack is
    /// named, never silently dropped.
    pub fn load_root(&mut self, root: &Path) -> Result<Vec<String>, BoxError> {
        let mut loaded = Vec::new();
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(loaded),
            Err(e) => return Err(format!("cannot list context-pack root {}: {e}", root.display()).into()),
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() && path.join("manifest.yaml").is_file() {
                dirs.push(path);
            }
        }
        dirs.sort();
        for dir in dirs {
            let pack = load_pack(&dir)?;
            let name = pack.name();
            loaded.push(name.clone());
            self.packs.insert(name, pack);
        }
        Ok(loaded)
    }

    pub fn get(&self, name: &str) -> Option<&ContextPack> {
        self.packs.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.packs.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.packs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp dir per test — the tests run in parallel, so the dir
    /// name must be unique per test (a shared dir races on remove_dir_all).
    fn test_pack_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("openrig-pack-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create pack dir");
        dir
    }

    fn write_pack(dir: &Path, manifest: &str, files: &[(&str, &str)]) {
        std::fs::write(dir.join("manifest.yaml"), manifest).expect("write manifest");
        for (name, content) in files {
            std::fs::write(dir.join(name), content).expect("write pack file");
        }
    }

    #[test]
    fn valid_pack_loads_with_contents() {
        let dir = test_pack_dir("valid");
        write_pack(
            &dir,
            "name: proof-packet\ntaxonomy: mission\nfiles:\n  - path: brief.md\n    role: prd\n    summary: The brief\n",
            &[("brief.md", "# Brief\n")],
        );
        let pack = load_pack(&dir).expect("load pack");
        assert_eq!(pack.name(), "proof-packet");
        assert_eq!(pack.manifest.taxonomy, "mission");
        assert_eq!(pack.contents.get("brief.md").map(String::as_str), Some("# Brief\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_taxonomy_is_refused() {
        let dir = test_pack_dir("bad-tax");
        write_pack(&dir, "taxonomy: secret\nfiles: []\n", &[]);
        let err = load_pack(&dir).expect_err("bad taxonomy must fail");
        assert!(err.to_string().contains("secret"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escaping_path_is_refused() {
        let dir = test_pack_dir("escape");
        write_pack(
            &dir,
            "taxonomy: lore\nfiles:\n  - path: ../etc/passwd\n    role: x\n",
            &[],
        );
        let err = load_pack(&dir).expect_err("escaping path must fail");
        assert!(err.to_string().contains("escapes"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_scans_a_root() {
        let root = test_pack_dir("library");
        let pack_dir = root.join("my-pack");
        std::fs::create_dir_all(&pack_dir).expect("create pack dir");
        write_pack(
            &pack_dir,
            "taxonomy: skills\nfiles:\n  - path: how.md\n    role: procedure\n",
            &[("how.md", "step 1\n")],
        );
        std::fs::create_dir_all(root.join("not-a-pack")).expect("create plain dir");
        let mut library = ContextPackLibrary::new();
        let loaded = library.load_root(&root).expect("load root");
        assert_eq!(loaded, vec!["my-pack".to_string()]);
        assert_eq!(library.len(), 1);
        assert!(library.get("my-pack").is_some());
        assert_eq!(resolve_context_pack_root(&root), root.join("context"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_loads_nothing() {
        let mut library = ContextPackLibrary::new();
        let loaded = library
            .load_root(&std::env::temp_dir().join("openrig-pack-test-absent"))
            .expect("missing root is empty, not an error");
        assert!(loaded.is_empty());
    }
}
