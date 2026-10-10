//! Embeds every file under `skills/` into the binary, so
//! `cpm-planner skills install` needs neither the repository nor the network.
//!
//! Generates `$OUT_DIR/embedded_skills.rs`: a sorted table of
//! `(path relative to skills/ with '/' separators, include_str!(absolute path))`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=skills");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let root = manifest_dir.join("skills");
    let mut files = Vec::new();
    collect(&root, &root, &mut files)?;
    files.sort();

    let mut table = String::from(
        "/// Every file under `skills/`, as (relative path, contents), sorted by path.\n\
         pub(crate) static EMBEDDED_SKILLS: &[(&str, &str)] = &[\n",
    );
    for (rel, abs) in &files {
        table.push_str(&format!(
            "    ({rel:?}, include_str!({abs:?})),\n",
            abs = abs.to_string_lossy()
        ));
    }
    table.push_str("];\n");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::write(out.join("embedded_skills.rs"), table)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walked paths are under the root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
    Ok(())
}
