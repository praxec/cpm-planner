use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::project::{ProjectRoot, validate_slug};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV: &str = "CPM_PROJECT_ROOT";

fn tmp(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cpm-project-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap()
}

fn root_with_git(label: &str) -> (PathBuf, ProjectRoot) {
    let d = tmp(label);
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let r = ProjectRoot::from_path(&d).unwrap();
    (d, r)
}

fn graph() -> PlanGraph {
    serde_json::from_value(serde_json::json!({
        "deliverables": [{
            "id": "a",
            "owned_files": ["a.rs"],
            "prerequisites": []
        }]
    }))
    .unwrap()
}

fn forged_ref(name: &str, variant: &str) -> cpm_planner::project::PlanFileRef {
    cpm_planner::project::PlanFileRef {
        name: name.into(),
        variant: variant.into(),
        rel_path: format!(".cpm-planner/plans/{name}/{variant}.json"),
    }
}

fn is_invalid_path<T: std::fmt::Debug>(r: Result<T, PlannerError>) -> bool {
    matches!(r, Err(PlannerError::InvalidPath { .. }))
}

fn with_env<R>(value: Option<&Path>, f: impl FnOnce() -> R) -> R {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var_os(ENV);
    // SAFETY: serialized by ENV_LOCK; no other test thread touches this var.
    unsafe {
        match value {
            Some(v) => std::env::set_var(ENV, v),
            None => std::env::remove_var(ENV),
        }
    }
    let out = f();
    unsafe {
        match old {
            Some(v) => std::env::set_var(ENV, v),
            None => std::env::remove_var(ENV),
        }
    }
    out
}

#[test]
fn discover_finds_git_root_from_nested_dir() {
    let (d, _) = root_with_git("git");
    let nested = d.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    let found = with_env(None, || ProjectRoot::discover(&nested));
    assert_eq!(found.unwrap().root(), d);
}

#[test]
fn discover_prefers_cpm_planner_dir() {
    let (d, _) = root_with_git("pref");
    let inner = d.join("sub");
    std::fs::create_dir_all(inner.join(".cpm-planner")).unwrap();
    let found = with_env(None, || ProjectRoot::discover(&inner.join(".cpm-planner")));
    assert_eq!(found.unwrap().root(), inner);
}

#[test]
fn env_override_wins() {
    let (d, _) = root_with_git("envgit");
    let other = tmp("envother");
    let found = with_env(Some(&other), || ProjectRoot::discover(&d));
    assert_eq!(found.unwrap().root(), other);
}

#[test]
fn resolve_rejects_parent_traversal() {
    let (_, r) = root_with_git("dots");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/../../etc/x.json")
    ));
}

#[test]
fn resolve_rejects_absolute_path() {
    let (d, r) = root_with_git("abs");
    let abs = d.join(".cpm-planner/plans/a/b.json");
    assert!(is_invalid_path(r.resolve_plan_file(abs.to_str().unwrap())));
}

#[cfg(unix)]
#[test]
fn resolve_rejects_symlink_escaping_root() {
    let (d, r) = root_with_git("sym");
    let outside = tmp("symout");
    std::fs::create_dir_all(d.join(".cpm-planner/plans")).unwrap();
    std::os::unix::fs::symlink(&outside, d.join(".cpm-planner/plans/evil")).unwrap();
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/evil/x.json")
    ));
}

#[cfg(unix)]
#[test]
fn write_refuses_to_follow_symlinked_file() {
    let (d, r) = root_with_git("symfile");
    let outside = tmp("symfileout");
    let victim = outside.join("victim.json");
    std::fs::write(&victim, "keep").unwrap();
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("b.json")).unwrap();
    let f = forged_ref("a", "b");
    assert!(is_invalid_path(r.write_graph(&f, &graph())));
}

#[cfg(unix)]
#[test]
fn write_leaves_symlink_target_untouched() {
    let (d, r) = root_with_git("symkeep");
    let outside = tmp("symkeepout");
    let victim = outside.join("victim.json");
    std::fs::write(&victim, "keep").unwrap();
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("b.json")).unwrap();
    let f = forged_ref("a", "b");
    let _ = r.write_graph(&f, &graph());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
}

#[test]
fn resolve_rejects_path_outside_plans_dir() {
    let (_, r) = root_with_git("outside");
    assert!(is_invalid_path(r.resolve_plan_file("src/lib.rs")));
}

#[test]
fn resolve_rejects_missing_json_extension() {
    let (_, r) = root_with_git("noext");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b.txt")
    ));
}

#[test]
fn resolve_rejects_extra_path_components() {
    let (_, r) = root_with_git("extra");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b/c.json")
    ));
}

#[test]
fn resolve_rejects_missing_variant_component() {
    let (_, r) = root_with_git("short");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a.json")
    ));
}

#[test]
fn resolve_rejects_backslash_separators() {
    let (_, r) = root_with_git("bslash");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner\\plans\\a\\b.json")
    ));
}

#[test]
fn resolve_rejects_nul_byte() {
    let (_, r) = root_with_git("nul");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b\0.json")
    ));
}

#[test]
fn resolve_parses_name_and_variant() {
    let (_, r) = root_with_git("parse");
    let f = r
        .resolve_plan_file(".cpm-planner/plans/my-plan/base.json")
        .unwrap();
    assert_eq!((f.name.as_str(), f.variant.as_str()), ("my-plan", "base"));
}

#[test]
fn plan_file_builds_canonical_rel_path() {
    let (_, r) = root_with_git("relpath");
    let f = r.plan_file("p", "v1").unwrap();
    assert_eq!(f.rel_path, ".cpm-planner/plans/p/v1.json");
}

#[test]
fn invalid_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", "Bad Name")));
}

#[test]
fn slug_with_leading_dot_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", ".hidden")));
}

#[test]
fn slug_longer_than_64_chars_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", &"a".repeat(65))));
}

#[test]
fn slug_of_64_chars_is_accepted() {
    assert!(validate_slug("name", &"a".repeat(64)).is_ok());
}

#[test]
fn empty_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("variant", "")));
}

#[test]
fn write_then_read_round_trips_graph() {
    let (_, r) = root_with_git("rt");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    let (g, _) = r.read_graph(&f).unwrap();
    assert_eq!(g.deliverables[0].id, "a");
}

#[test]
fn read_hash_matches_write_hash() {
    let (_, r) = root_with_git("hash");
    let f = r.plan_file("p", "v").unwrap();
    let w = r.write_graph(&f, &graph()).unwrap();
    let (_, h) = r.read_graph(&f).unwrap();
    assert_eq!(w, h);
}

#[test]
fn hash_is_sha256_hex_of_file_bytes() {
    use sha2::{Digest, Sha256};
    let (d, r) = root_with_git("sha");
    let f = r.plan_file("p", "v").unwrap();
    let w = r.write_graph(&f, &graph()).unwrap();
    let bytes = std::fs::read(d.join(&f.rel_path)).unwrap();
    assert_eq!(w, format!("{:x}", Sha256::digest(&bytes)));
}

#[test]
fn written_file_ends_with_newline() {
    let (d, r) = root_with_git("nl");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    assert!(
        std::fs::read_to_string(d.join(&f.rel_path))
            .unwrap()
            .ends_with('\n')
    );
}

#[test]
fn write_creates_directories() {
    let (d, r) = root_with_git("mk");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    assert!(d.join(".cpm-planner/plans/p/v.json").is_file());
}

#[test]
fn write_leaves_no_temp_files_behind() {
    let (d, r) = root_with_git("atomic");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    r.write_graph(&f, &graph()).unwrap();
    let n = std::fs::read_dir(d.join(".cpm-planner/plans/p"))
        .unwrap()
        .count();
    assert_eq!(n, 1);
}

#[test]
fn list_plan_files_is_sorted() {
    let (_, r) = root_with_git("list");
    for (n, v) in [("b", "x"), ("a", "z"), ("a", "y")] {
        let f = r.plan_file(n, v).unwrap();
        r.write_graph(&f, &graph()).unwrap();
    }
    let got: Vec<_> = r
        .list_plan_files()
        .unwrap()
        .into_iter()
        .map(|f| (f.name, f.variant))
        .collect();
    let want: Vec<_> = [("a", "y"), ("a", "z"), ("b", "x")]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn list_plan_files_is_empty_without_plans_dir() {
    let (_, r) = root_with_git("nolist");
    assert!(r.list_plan_files().unwrap().is_empty());
}

#[test]
fn list_plan_files_ignores_non_plan_entries() {
    let (d, r) = root_with_git("junk");
    let f = r.plan_file("a", "b").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    std::fs::write(d.join(".cpm-planner/plans/a/notes.txt"), "x").unwrap();
    std::fs::write(d.join(".cpm-planner/plans/stray.json"), "x").unwrap();
    assert_eq!(r.list_plan_files().unwrap().len(), 1);
}

#[test]
fn project_key_is_canonical_root_string() {
    let (d, r) = root_with_git("key");
    assert_eq!(r.project_key(), d.to_str().unwrap());
}

#[test]
fn from_path_rejects_missing_directory() {
    let d = tmp("missing");
    assert!(ProjectRoot::from_path(&d.join("nope")).is_err());
}

#[cfg(unix)]
#[test]
fn plan_file_rejects_symlinked_plan_file() {
    let (d, r) = root_with_git("symref");
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(d.join("elsewhere.json"), dir.join("b.json")).unwrap();
    assert!(is_invalid_path(r.plan_file("a", "b")));
}
