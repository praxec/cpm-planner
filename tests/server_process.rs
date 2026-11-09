//! The real binary over MCP stdio: read-only startup (#120), the database log
//! line (#117), the `max_count` default (#118) and the gate deliverable flow.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(db: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
            .env("CPM_PLANNER_DB", db)
            .env("CPM_PROJECT_ROOT", db.parent().expect("parent"))
            .env_remove("CPM_PLANNER_STATE_MODE")
            .env_remove("OPENROUTER_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        server.request(
            "initialize",
            json!({ "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" } }),
        );
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        server
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        serde_json::from_str(&line).unwrap_or_else(|_| {
            let mut stderr = String::new();
            if let Some(mut err) = self.child.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            panic!("server replied: {line:?}; stderr: {stderr}")
        })
    }

    /// The tool's parsed result, or `Err` with its error message.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let reply = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        if let Some(error) = reply.get("error") {
            return Err(error.to_string());
        }
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        if reply["result"]["isError"] == true {
            return Err(text.to_string());
        }
        serde_json::from_str(text).map_err(|e| format!("{e}: {text}"))
    }

    /// Close stdin, wait for the exit and return everything on stderr.
    fn stop(self) -> String {
        let Self {
            mut child,
            stdin,
            stdout,
            ..
        } = self;
        drop(stdin);
        drop(stdout);
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        child.wait().expect("exit");
        stderr
    }
}

fn graph() -> Value {
    json!({ "deliverables": [
        { "id": "code", "owned_files": ["a.rs"], "prerequisites": [],
          "estimated_effort_hours": 1.0,
          "metadata": { "artifact": "a.rs", "owner": "agent" } },
        { "id": "other", "owned_files": ["b.rs"], "prerequisites": [],
          "estimated_effort_hours": 1.0,
          "metadata": { "artifact": "b.rs", "owner": "agent" } },
        { "id": "regression", "owned_files": [],
          "prerequisites": [{ "id": "code", "consumes": "a.rs" }, { "id": "other", "consumes": "b.rs" }],
          "duration_hours": 1.0,
          "metadata": { "artifact": "green run", "owner": "ci", "kind": "gate" } }
    ]})
}

fn submit(server: &mut Server) -> String {
    let result = server
        .call("plan.submit", json!({ "graph": graph() }))
        .expect("submit");
    result["plan_id"].as_str().expect("plan_id").to_string()
}

fn leased_ids(cohort: &Value) -> Vec<String> {
    cohort["deliverables"]
        .as_array()
        .expect("deliverables")
        .iter()
        .map(|d| d["id"].as_str().expect("id").to_string())
        .collect()
}

#[test]
fn the_database_log_line_is_the_marker_then_the_path_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("planner.db");
    let stderr = Server::start(&db).stop();
    let line = stderr
        .lines()
        .find(|l| l.contains("opening planner database"))
        .expect("log line")
        .to_string();
    assert!(
        line.ends_with(&format!("opening planner database db={}", db.display())),
        "log line: {line}"
    );
}

#[test]
fn acquire_without_max_count_leases_one_deliverable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start(&dir.path().join("planner.db"));
    let plan_id = submit(&mut server);
    let cohort = server
        .call(
            "plan.acquire_cohort",
            json!({ "plan_id": plan_id, "caller_id": "w" }),
        )
        .expect("acquire");
    assert_eq!(leased_ids(&cohort).len(), 1);
}

#[test]
fn acquire_without_max_count_leases_every_requested_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start(&dir.path().join("planner.db"));
    let plan_id = submit(&mut server);
    let cohort = server
        .call(
            "plan.acquire_cohort",
            json!({ "plan_id": plan_id, "caller_id": "w", "ids": ["code", "other"] }),
        )
        .expect("acquire");
    assert_eq!(leased_ids(&cohort).len(), 2);
}

/// Complete the work, then return the server and plan with the gate ready.
fn gate_ready(dir: &Path) -> (Server, String) {
    let mut server = Server::start(&dir.join("planner.db"));
    let plan_id = submit(&mut server);
    server
        .call(
            "plan.acquire_cohort",
            json!({ "plan_id": plan_id, "caller_id": "w", "ids": ["code", "other"] }),
        )
        .expect("acquire");
    for id in ["code", "other"] {
        server
            .call(
                "plan.mark_status",
                json!({ "plan_id": plan_id, "deliverable_id": id, "caller_id": "w",
                        "status": { "status": "complete" } }),
            )
            .expect("complete");
    }
    (server, plan_id)
}

#[test]
fn a_ready_gate_is_never_leased() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut server, plan_id) = gate_ready(dir.path());
    let cohort = server
        .call(
            "plan.acquire_cohort",
            json!({ "plan_id": plan_id, "caller_id": "w", "ids": ["regression"] }),
        )
        .expect("acquire");
    assert_eq!(cohort["blocked"][0]["code"], "MANUAL");
}

#[test]
fn a_gate_before_its_work_is_complete_cannot_be_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start(&dir.path().join("planner.db"));
    let plan_id = submit(&mut server);
    let accepted = server.call(
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "regression", "accepted_by": "ci",
                "evidence": "run 1" }),
    );
    assert!(accepted.is_err(), "{accepted:?}");
}

#[test]
fn a_red_run_recorded_as_in_progress_can_still_be_accepted_when_green() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut server, plan_id) = gate_ready(dir.path());
    server
        .call(
            "plan.mark_status",
            json!({ "plan_id": plan_id, "deliverable_id": "regression", "caller_id": "ci",
                    "status": { "status": "in_progress" }, "evidence": "run 1 red" }),
        )
        .expect("record the red run");
    server
        .call(
            "plan.accept",
            json!({ "plan_id": plan_id, "deliverable_id": "regression", "accepted_by": "ci",
                    "evidence": "run 2 green" }),
        )
        .expect("accept");
    let status = server
        .call("plan.status", json!({ "plan_id": plan_id }))
        .expect("status");
    assert_eq!(status["plan_complete"], true);
}

/// A closed database holding an expired lease, made read-only.
#[cfg(unix)]
fn readonly_with_expired_lock(dir: &Path) -> (std::path::PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let db = dir.join("planner.db");
    let mut server = Server::start(&db);
    let plan_id = submit(&mut server);
    server
        .call(
            "plan.acquire_cohort",
            json!({ "plan_id": plan_id, "caller_id": "w", "ids": ["code"], "ttl_seconds": 1 }),
        )
        .expect("acquire");
    server.stop();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).expect("chmod db");
    (db, plan_id)
}

#[cfg(unix)]
#[test]
fn status_reads_a_readonly_database_with_an_expired_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, plan_id) = readonly_with_expired_lock(dir.path());
    let mut server = Server::start(&db);
    let status = server.call("plan.status", json!({ "plan_id": plan_id }));
    assert!(status.is_ok(), "{status:?}");
}

#[cfg(unix)]
#[test]
fn list_reads_a_readonly_database_with_an_expired_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, _) = readonly_with_expired_lock(dir.path());
    let mut server = Server::start(&db);
    let list = server.call("plan.list", json!({}));
    assert!(list.is_ok(), "{list:?}");
}

#[cfg(unix)]
#[test]
fn reading_a_readonly_database_changes_no_byte_of_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, plan_id) = readonly_with_expired_lock(dir.path());
    let before = std::fs::read(&db).expect("read");
    let mut server = Server::start(&db);
    server
        .call("plan.status", json!({ "plan_id": plan_id }))
        .expect("status");
    server.stop();
    assert!(before == std::fs::read(&db).expect("read"));
}

#[cfg(unix)]
#[test]
fn a_lease_on_a_readonly_database_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, plan_id) = readonly_with_expired_lock(dir.path());
    let mut server = Server::start(&db);
    let acquired = server.call(
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "w", "ids": ["other"] }),
    );
    assert!(acquired.is_err(), "{acquired:?}");
}

#[cfg(unix)]
#[test]
fn status_reads_a_readonly_database_in_a_readonly_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state");
    std::fs::create_dir(&state).expect("mkdir");
    let (db, plan_id) = readonly_with_expired_lock(&state);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).expect("chmod dir");
    let mut server = Server::start(&db);
    let status = server.call("plan.status", json!({ "plan_id": plan_id }));
    server.stop();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(status.is_ok(), "{status:?}");
}
