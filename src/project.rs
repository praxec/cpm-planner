//! Project-root discovery and confined plan-file paths.
//!
//! Plan files live at `<root>/.cpm-planner/plans/<name>/<variant>.json`.
//! Every caller-supplied path is relative to the root and is validated
//! component by component: no `..`, no absolute paths, no backslashes, no
//! extra components, slug-checked name/variant, and no symlinks anywhere on
//! the plan path. Writes are atomic (temp file in the same directory, then
//! rename) and never follow a symlink.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::plan::{PlanGraph, PlannerError};

/// Environment variable overriding repo-root discovery.
pub const PROJECT_ROOT_ENV: &str = "CPM_PROJECT_ROOT";

const PLANNER_DIR: &str = ".cpm-planner";
const PLANS_SUBDIR: &str = "plans";

/// A validated plan file location (not necessarily existing yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileRef {
    pub name: String,
    pub variant: String,
    /// `.cpm-planner/plans/<name>/<variant>.json`, relative to the root.
    pub rel_path: String,
}

/// A canonical project root.
#[derive(Debug, Clone)]
pub struct ProjectRoot {
    root: PathBuf,
}

fn invalid(reason: impl Into<String>) -> PlannerError {
    PlannerError::InvalidPath {
        reason: reason.into(),
    }
}

fn backend(err: impl Into<anyhow::Error>) -> PlannerError {
    PlannerError::BackendError(err.into())
}

/// Validate a `name` / `variant`: `^[a-z0-9][a-z0-9._-]{0,63}$`.
pub fn validate_slug(kind: &str, s: &str) -> Result<(), PlannerError> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || (i > 0 && matches!(b, b'.' | b'_' | b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{kind} {s:?} must match ^[a-z0-9][a-z0-9._-]{{0,63}}$"
        )))
    }
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

impl ProjectRoot {
    /// `CPM_PROJECT_ROOT` if set, else the nearest ancestor of `cwd` (itself
    /// included) containing `.cpm-planner/` or `.git`.
    pub fn discover(cwd: &Path) -> Option<ProjectRoot> {
        if let Some(v) = std::env::var_os(PROJECT_ROOT_ENV).filter(|v| !v.is_empty()) {
            return Self::from_path(Path::new(&v)).ok();
        }
        let start = cwd.canonicalize().ok()?;
        start
            .ancestors()
            .find(|d| d.join(PLANNER_DIR).is_dir() || d.join(".git").exists())
            .map(|d| ProjectRoot {
                root: d.to_path_buf(),
            })
    }

    /// Build a root from an existing directory (canonicalized).
    pub fn from_path(root: &Path) -> Result<ProjectRoot, PlannerError> {
        let root = root
            .canonicalize()
            .map_err(|e| invalid(format!("project root {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(invalid(format!(
                "project root {} is not a directory",
                root.display()
            )));
        }
        Ok(ProjectRoot { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical root as a UTF-8 string (lossy for non-UTF-8 paths).
    pub fn project_key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.root.join(PLANNER_DIR).join(PLANS_SUBDIR)
    }

    /// Validate a root-relative path of the exact form
    /// `.cpm-planner/plans/<name>/<variant>.json`.
    pub fn resolve_plan_file(&self, rel: &str) -> Result<PlanFileRef, PlannerError> {
        if rel.contains('\0') || rel.contains('\\') {
            return Err(invalid("path contains NUL or backslash"));
        }
        if rel.starts_with('/') || Path::new(rel).is_absolute() {
            return Err(invalid("absolute paths are not allowed"));
        }
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.contains(&"..") {
            return Err(invalid("`..` is not allowed"));
        }
        let [planner, plans, name, file] = parts.as_slice() else {
            return Err(invalid(format!(
                "path must be {PLANNER_DIR}/{PLANS_SUBDIR}/<name>/<variant>.json, got {rel:?}"
            )));
        };
        if *planner != PLANNER_DIR || *plans != PLANS_SUBDIR {
            return Err(invalid(format!(
                "path must be inside {PLANNER_DIR}/{PLANS_SUBDIR}/, got {rel:?}"
            )));
        }
        let Some(variant) = file.strip_suffix(".json") else {
            return Err(invalid(format!("plan file {file:?} must end in .json")));
        };
        self.plan_file(name, variant)
    }

    /// Build and confinement-check the ref for `<name>/<variant>`.
    pub fn plan_file(&self, name: &str, variant: &str) -> Result<PlanFileRef, PlannerError> {
        validate_slug("name", name)?;
        validate_slug("variant", variant)?;
        let f = PlanFileRef {
            name: name.to_string(),
            variant: variant.to_string(),
            rel_path: format!("{PLANNER_DIR}/{PLANS_SUBDIR}/{name}/{variant}.json"),
        };
        self.check_confined(&f)?;
        Ok(f)
    }

    /// No component of the plan path may be a symlink, and whatever part of
    /// it exists must canonicalize to a location inside the plans dir.
    fn check_confined(&self, f: &PlanFileRef) -> Result<(), PlannerError> {
        let planner = self.root.join(PLANNER_DIR);
        let plans = planner.join(PLANS_SUBDIR);
        let dir = plans.join(&f.name);
        let file = self.abs(f);
        for p in [&planner, &plans, &dir, &file] {
            if is_symlink(p) {
                return Err(invalid(format!(
                    "{} is a symlink; symlinks are not allowed",
                    p.display()
                )));
            }
        }
        for (p, anchor) in [(&planner, &self.root), (&plans, &self.root), (&dir, &plans)] {
            if let Ok(c) = p.canonicalize() {
                let anchor_c = anchor.canonicalize().map_err(backend)?;
                if !c.starts_with(&anchor_c) {
                    return Err(invalid(format!("{} escapes the project root", p.display())));
                }
            }
        }
        Ok(())
    }

    fn abs(&self, f: &PlanFileRef) -> PathBuf {
        self.root.join(&f.rel_path)
    }

    /// All plan files, sorted by (name, variant). Non-conforming entries and
    /// symlinks are ignored.
    pub fn list_plan_files(&self) -> Result<Vec<PlanFileRef>, PlannerError> {
        let plans = self.plans_dir();
        let mut out = Vec::new();
        let Ok(names) = std::fs::read_dir(&plans) else {
            return Ok(out);
        };
        for n in names.flatten() {
            let Some(name) = n.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !n.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(n.path()) else {
                continue;
            };
            for f in files.flatten() {
                let Some(file) = f.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Some(variant) = file.strip_suffix(".json") else {
                    continue;
                };
                if !f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                if let Ok(r) = self.plan_file(&name, variant) {
                    out.push(r);
                }
            }
        }
        out.sort_by(|a, b| (&a.name, &a.variant).cmp(&(&b.name, &b.variant)));
        Ok(out)
    }

    /// Read a plan and the sha256 hex of its exact bytes.
    pub fn read_graph(&self, f: &PlanFileRef) -> Result<(PlanGraph, String), PlannerError> {
        let f = self.plan_file(&f.name, &f.variant)?;
        let bytes = std::fs::read(self.abs(&f)).map_err(backend)?;
        let graph = serde_json::from_slice(&bytes).map_err(|e| PlannerError::InvalidGraph {
            reason: format!("{}: {e}", f.rel_path),
        })?;
        Ok((graph, hash_hex(&bytes)))
    }

    /// Atomically write pretty JSON plus a trailing newline; returns the
    /// content hash. Never follows a symlink.
    pub fn write_graph(&self, f: &PlanFileRef, g: &PlanGraph) -> Result<String, PlannerError> {
        let f = self.plan_file(&f.name, &f.variant)?;
        let mut bytes = serde_json::to_vec_pretty(g).map_err(backend)?;
        bytes.push(b'\n');
        let target = self.abs(&f);
        let dir = self.plans_dir().join(&f.name);
        std::fs::create_dir_all(&dir).map_err(backend)?;
        // Re-check after creating directories (narrows the race window).
        self.check_confined(&f)?;
        let tmp = dir.join(format!(".{}.tmp-{}", f.variant, uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &target)
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(backend(e));
        }
        Ok(hash_hex(&bytes))
    }
}

fn hash_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
