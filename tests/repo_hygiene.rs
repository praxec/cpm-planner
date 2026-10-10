use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

const BINARY_EXTENSIONS: &[&str] = &[
    "png", "ico", "jpg", "gif", "db", "sqlite", "zip", "gz", "tgz",
];

fn is_binary_path(path: &str) -> bool {
    match Path::new(path).extension().and_then(|ext| ext.to_str()) {
        Some(ext) => BINARY_EXTENSIONS
            .iter()
            .any(|binary| ext.eq_ignore_ascii_case(binary)),
        None => false,
    }
}

fn contains_crlf(path: &Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => bytes.windows(2).any(|window| window == b"\r\n"),
        Err(_) => false,
    }
}

#[test]
fn tracked_text_files_are_lf() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let output = match Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(manifest_dir)
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return,
    };

    let tracked = String::from_utf8_lossy(&output.stdout);
    let offenders: Vec<&str> = tracked
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter(|path| !is_binary_path(path))
        .filter(|path| contains_crlf(&Path::new(manifest_dir).join(path)))
        .collect();

    assert!(
        offenders.is_empty(),
        "tracked text files contain CRLF line endings: {offenders:?}"
    );
}

fn load_issue_yaml(name: &str) -> Result<serde_yaml_ng::Value, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github/ISSUE_TEMPLATE")
        .join(name);
    let text = std::fs::read_to_string(&path).map_err(|err| format!("{name}: {err}"))?;
    serde_yaml_ng::from_str(&text).map_err(|err| format!("{name}: {err}"))
}

const ISSUE_FORMS: &[&str] = &["bug_report.yml", "feature_request.yml"];

#[test]
fn issue_forms_are_valid_yaml() {
    let invalid: Vec<String> = ISSUE_FORMS
        .iter()
        .chain(&["config.yml"])
        .filter_map(|name| load_issue_yaml(name).err())
        .collect();

    assert!(invalid.is_empty(), "invalid issue templates: {invalid:?}");
}

#[test]
fn issue_config_disables_blank_issues() {
    let config = load_issue_yaml("config.yml").expect("config.yml parses");
    let blank_disabled = config["blank_issues_enabled"].as_bool() == Some(false);
    let has_links = config["contact_links"]
        .as_sequence()
        .is_some_and(|links| !links.is_empty());

    assert!(
        blank_disabled && has_links,
        "config.yml must set blank_issues_enabled: false and non-empty contact_links"
    );
}

#[test]
fn issue_forms_have_name_description_and_typed_body() {
    let offenders: Vec<&str> = ISSUE_FORMS
        .iter()
        .copied()
        .filter(|name| {
            let Ok(form) = load_issue_yaml(name) else {
                return true;
            };
            let has_text = |key: &str| form[key].as_str().is_some_and(|s| !s.is_empty());
            let typed_body = form["body"].as_sequence().is_some_and(|body| {
                !body.is_empty() && body.iter().all(|item| item["type"].as_str().is_some())
            });
            !(has_text("name") && has_text("description") && typed_body)
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "issue forms missing name, description or typed body items: {offenders:?}"
    );
}

/// Markdown files whose relative links are checked: README.md plus every
/// `.md` under `docs/`, except the design history in `docs/superpowers/`.
/// Directories that cannot be listed are returned as errors.
fn linked_markdown_files(root: &Path) -> (Vec<std::path::PathBuf>, Vec<String>) {
    fn walk(dir: &Path, skip: &Path, out: &mut Vec<std::path::PathBuf>, errors: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                errors.push(format!("{}: cannot list directory ({err})", dir.display()));
                return;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(err) => {
                    errors.push(format!("{}: cannot read entry ({err})", dir.display()));
                    continue;
                }
            };
            if path == skip {
                continue;
            }
            if path.is_dir() {
                walk(&path, skip, out, errors);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                out.push(path);
            }
        }
    }
    let mut files = vec![root.join("README.md")];
    let mut errors = Vec::new();
    walk(
        &root.join("docs"),
        &root.join("docs").join("superpowers"),
        &mut files,
        &mut errors,
    );
    files.sort();
    (files, errors)
}

/// Lines of a Markdown file outside fenced code blocks.
fn prose_lines(text: &str) -> Vec<&str> {
    let mut in_fence = false;
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                return false;
            }
            !in_fence
        })
        .collect()
}

/// `line` with inline code spans removed, so `[a](b)` inside backticks is
/// not taken for a link.
fn without_code_spans(line: &str) -> String {
    line.split('`').step_by(2).collect::<Vec<_>>().join(" ")
}

/// Every link target `(...)` that follows a `]` on the line.
fn link_targets(line: &str) -> Vec<String> {
    let line = without_code_spans(line);
    line.match_indices("](")
        .filter_map(|(at, _)| {
            let rest = &line[at + 2..];
            let end = rest.find(')')?;
            let target = rest[..end].split_whitespace().next()?;
            Some(target.trim_matches(|c| c == '<' || c == '>').to_string())
        })
        .collect()
}

/// GitHub heading anchors of a Markdown file: lowercase, spaces become `-`,
/// punctuation other than `-` and `_` is dropped, repeats get `-1`, `-2`, ...
fn heading_anchors(text: &str) -> HashSet<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut anchors = HashSet::new();
    for line in prose_lines(text) {
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if hashes == 0 || hashes > 6 || !line[hashes..].starts_with(' ') {
            continue;
        }
        let slug: String = line[hashes..]
            .trim()
            .to_lowercase()
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                '-' | '_' => Some(c),
                c if c.is_alphanumeric() => Some(c),
                _ => None,
            })
            .collect();
        let count = seen.entry(slug.clone()).or_insert(0);
        let anchor = if *count == 0 {
            slug.clone()
        } else {
            format!("{slug}-{count}")
        };
        *count += 1;
        anchors.insert(anchor);
    }
    anchors
}

/// Why a relative link `target` written in `file` does not resolve, if it
/// does not.
fn broken_link_reason(file: &Path, target: &str) -> Option<String> {
    if target.is_empty() || target.contains("://") || target.starts_with("mailto:") {
        return None;
    }
    let (path_part, anchor) = match target.split_once('#') {
        Some((path, anchor)) => (path, Some(anchor)),
        None => (target, None),
    };
    let resolved = if path_part.is_empty() {
        file.to_path_buf()
    } else {
        file.parent()?.join(path_part)
    };
    if !resolved.exists() {
        return Some("missing file".to_string());
    }
    let anchor = anchor.filter(|a| !a.is_empty())?;
    if resolved.extension().is_none_or(|ext| ext != "md") {
        return None;
    }
    let text = match std::fs::read_to_string(&resolved) {
        Ok(text) => text,
        Err(err) => return Some(format!("cannot read target ({err})")),
    };
    if heading_anchors(&text).contains(anchor) {
        None
    } else {
        Some(format!("no heading for #{anchor}"))
    }
}

#[test]
fn readme_links_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (files, mut offenders) = linked_markdown_files(root);
    for file in &files {
        let shown = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) => {
                offenders.push(format!("{shown}: cannot read file ({err})"));
                continue;
            }
        };
        offenders.extend(
            prose_lines(&text)
                .into_iter()
                .flat_map(link_targets)
                .filter_map(|target| {
                    broken_link_reason(file, &target)
                        .map(|why| format!("{shown}: {target} ({why})"))
                }),
        );
    }

    assert!(
        offenders.is_empty(),
        "broken relative links in Markdown docs: {offenders:#?}"
    );
}
