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

#[test]
fn issue_forms_are_valid_yaml() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/ISSUE_TEMPLATE");
    let invalid: Vec<String> = ["bug_report.yml", "feature_request.yml", "config.yml"]
        .iter()
        .filter(|name| {
            std::fs::read_to_string(dir.join(name))
                .map_err(|err| err.to_string())
                .and_then(|text| {
                    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text)
                        .map_err(|err| err.to_string())
                })
                .is_err()
        })
        .map(|name| name.to_string())
        .collect();

    assert!(
        invalid.is_empty(),
        "invalid or missing issue forms: {invalid:?}"
    );
}
