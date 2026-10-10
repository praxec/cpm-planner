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
