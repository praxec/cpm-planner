//! Embeds the agent skills under `skills/` into the binary, so
//! `cpm-planner skills install` needs neither the repository nor the network.
//!
//! Generates `$OUT_DIR/embedded_skills.rs` with two sorted tables:
//! - `EMBEDDED_SKILLS`: (path relative to `skills/` with `/` separators,
//!   `include_str!(absolute path)`);
//! - `SKILL_DESCRIPTIONS`: (skill name, front-matter `description`).
//!
//! Only `*.md`, `*.json` and `*.toml` files are embedded. Dotfiles and editor
//! droppings (`*~`, `*.swp`) are skipped; any other file, a symlink, or a
//! SKILL.md whose front matter is malformed fails the build.

use std::fs;
use std::path::{Path, PathBuf};

const DESCRIPTION_LIMIT: usize = 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=skills");
    if let Err(msg) = run() {
        eprintln!("error: embedding skills/: {msg}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR unset")?);
    let root = manifest_dir.join("skills");
    let mut files = Vec::new();
    collect(&root, &root, &mut files)?;
    files.sort();

    let mut descriptions = Vec::new();
    for (rel, abs) in &files {
        let Some(name) = rel.strip_suffix("/SKILL.md").filter(|n| !n.contains('/')) else {
            continue;
        };
        let text = fs::read_to_string(abs).map_err(|e| format!("skills/{rel}: {e}"))?;
        let (front_name, description) =
            front_matter(&text).map_err(|e| format!("skills/{rel}: {e}"))?;
        if front_name != name {
            return Err(format!(
                "skills/{rel}: front-matter name `{front_name}` must equal the directory `{name}`"
            ));
        }
        let plain = text
            .lines()
            .find_map(|l| l.strip_prefix("description:"))
            .map(str::trim)
            .filter(|raw| !raw.starts_with(['"', '\'']));
        if plain.is_some_and(|raw| raw.contains(": ") || raw.contains(" #")) {
            // Lenient readers take the whole line; strict YAML parsers reject
            // `: ` and read ` #` as a comment. Warn until the skill is quoted.
            println!(
                "cargo:warning=skills/{rel}: `description` is a plain YAML scalar containing \
                 `: ` or ` #`, which strict YAML parsers reject; quote the value"
            );
        }
        descriptions.push((name.to_string(), description));
    }

    let mut table = String::from(
        "/// Every embedded file under `skills/`, as (relative path, contents), sorted by path.\n\
         pub(crate) static EMBEDDED_SKILLS: &[(&str, &str)] = &[\n",
    );
    for (rel, abs) in &files {
        table.push_str(&format!(
            "    ({rel:?}, include_str!({:?})),\n",
            abs.to_string_lossy()
        ));
    }
    table.push_str(
        "];\n\n/// Each skill's front-matter `description`, as (skill name, description).\n\
         pub(crate) static SKILL_DESCRIPTIONS: &[(&str, &str)] = &[\n",
    );
    for (name, description) in &descriptions {
        table.push_str(&format!("    ({name:?}, {description:?}),\n"));
    }
    table.push_str("];\n");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR unset")?);
    fs::write(out.join("embedded_skills.rs"), table).map_err(|e| format!("writing the table: {e}"))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("{}: non-UTF-8 file name {n:?}", dir.display()))?;
        if name.starts_with('.') || name.ends_with('~') || name.ends_with(".swp") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| format!("{} is outside skills/", path.display()))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let kind = entry
            .file_type()
            .map_err(|e| format!("skills/{rel}: {e}"))?;
        if kind.is_symlink() {
            return Err(format!(
                "skills/{rel} is a symlink; only real files are embedded"
            ));
        } else if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if !matches!(ext, "md" | "json" | "toml") {
                return Err(format!(
                    "skills/{rel}: only .md, .json and .toml files are embedded; move or rename it"
                ));
            }
            out.push((rel, path));
        } else {
            return Err(format!("skills/{rel} is not a regular file"));
        }
    }
    Ok(())
}

/// The `name` and `description` of a SKILL.md front matter. Accepts plain,
/// single-quoted and double-quoted single-line scalars.
fn front_matter(text: &str) -> Result<(String, String), String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err("missing `---` front matter".into());
    }
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        body.push(line);
    }
    if !closed {
        return Err("front matter has no closing `---`".into());
    }
    let field = |key: &str| -> Result<String, String> {
        let prefix = format!("{key}:");
        let idx = body
            .iter()
            .position(|l| l.starts_with(&prefix))
            .ok_or_else(|| format!("front matter has no `{key}`"))?;
        if body
            .get(idx + 1)
            .is_some_and(|next| next.starts_with([' ', '\t']))
        {
            return Err(format!("`{key}` must be a single-line scalar"));
        }
        scalar(key, body[idx][prefix.len()..].trim())
    };
    let name = field("name")?;
    let description = field("description")?;
    if description.is_empty() || description.chars().count() > DESCRIPTION_LIMIT {
        return Err(format!(
            "`description` must be 1..={DESCRIPTION_LIMIT} characters"
        ));
    }
    Ok((name, description))
}

fn scalar(key: &str, raw: &str) -> Result<String, String> {
    if let Some(inner) = raw.strip_prefix('"') {
        let inner = inner
            .strip_suffix('"')
            .ok_or_else(|| format!("`{key}`: unterminated double-quoted scalar"))?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    other => {
                        return Err(format!(
                            "`{key}`: unsupported escape `\\{}`",
                            other.unwrap_or(' ')
                        ));
                    }
                },
                '"' => {
                    return Err(format!(
                        "`{key}`: unescaped `\"` inside a double-quoted scalar"
                    ));
                }
                c => out.push(c),
            }
        }
        Ok(out)
    } else if let Some(inner) = raw.strip_prefix('\'') {
        let inner = inner
            .strip_suffix('\'')
            .ok_or_else(|| format!("`{key}`: unterminated single-quoted scalar"))?;
        if inner.replace("''", "").contains('\'') {
            return Err(format!(
                "`{key}`: unescaped `'` inside a single-quoted scalar"
            ));
        }
        Ok(inner.replace("''", "'"))
    } else if raw.starts_with(['|', '>']) {
        Err(format!(
            "`{key}`: block scalars are not supported; use a single line"
        ))
    } else if raw.starts_with(['[', '{', '&', '*', '!', '%', '@', '`', '"', '\'']) {
        Err(format!(
            "`{key}`: a plain scalar cannot start with `{}`; quote it",
            &raw[..1]
        ))
    } else {
        Ok(raw.to_string())
    }
}
