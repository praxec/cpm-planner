//! Compile bounded release workflow metadata into ordinary, auditable plans.
//!
//! This module is pure: it neither queries a forge nor changes execution state.
//! Collection sources and run-history receipts are supplied by the caller.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::lint::{LintFinding, LintReport, Severity};
use crate::plan::{Deliverable, Estimate, OwnedFile, PlanGraph, PlannerError, Prerequisite};

/// The supported input contract. Unknown versions fail closed.
pub const SCHEMA_VERSION: u32 = 1;
/// Input files and CLI graph files are bounded before parsing.
pub const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// A retained source whose expansion is capped, rather than an open-ended glob.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpansionSource {
    pub uri: String,
    pub revision: String,
    pub max_items: usize,
}

/// Every expanding collection declares its exact count or a bounded source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion<T> {
    pub items: Vec<T>,
    #[serde(default)]
    pub cardinality: Option<usize>,
    #[serde(default)]
    pub source: Option<ExpansionSource>,
}

impl<T> Default for Expansion<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            cardinality: Some(0),
            source: None,
        }
    }
}

/// Explicit estimate provenance; no compiler-supplied duration guesses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseEstimate {
    pub hours: f32,
    pub source: String,
    /// Caller confidence in [0, 1], retained verbatim (not a probability of success).
    pub confidence: f32,
}

/// Expansion stages, also used as keys for fallback estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Pr,
    Check,
    Join,
    ApprovalWait,
    Approval,
    Merge,
    Producer,
    Receipt,
    Artifact,
    Acceptance,
}

/// The outcome recorded by a retained run or a planned proof attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Planned,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

/// History attaches to a deterministic generated node id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedRun {
    pub node_id: String,
    pub duration_hours: f32,
    pub outcome: RunOutcome,
    pub evidence: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionEdge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheck {
    pub id: String,
    pub workflow: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub id: String,
    pub environment: String,
}

/// One protected PR hop. `depends_on` names promotion ids in this repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Promotion {
    pub id: String,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub checks: Expansion<RequiredCheck>,
    #[serde(default)]
    pub approvals: Expansion<Approval>,
}

/// A retained change-decision receipt, including a no-change campaign skip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeTrigger {
    pub matched: bool,
    pub evidence: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofAttempt {
    pub id: String,
    pub outcome: RunOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifact {
    pub id: String,
    pub path: String,
}

/// A producer starts only after its promotion merge, and retains every retry receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub id: String,
    pub workflow: String,
    pub promotion: String,
    #[serde(default)]
    pub trigger: Option<ChangeTrigger>,
    pub attempts: Expansion<ProofAttempt>,
    pub artifacts: Expansion<RetainedArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// Stable local id, separate from the forge repository name.
    pub id: String,
    pub repository: String,
    pub allowed_edges: Vec<PromotionEdge>,
    pub promotions: Expansion<Promotion>,
    #[serde(default)]
    pub producers: Expansion<Producer>,
}

/// Central acceptance consumes explicitly named repo/promotion/producer artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceDependency {
    pub repository: String,
    pub promotion: String,
    #[serde(default)]
    pub producer: Option<String>,
    #[serde(default)]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralAcceptance {
    pub id: String,
    pub dependencies: Expansion<AcceptanceDependency>,
}

/// Versioned, offline importer contract. All expansion inputs are retained here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWorkflow {
    pub schema_version: u32,
    pub repositories: Expansion<Repository>,
    pub estimates: BTreeMap<Stage, ReleaseEstimate>,
    pub acceptance: CentralAcceptance,
    #[serde(default)]
    pub history: Vec<RetainedRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCompilation {
    pub schema_version: u32,
    pub policy_digest: String,
    pub graph: PlanGraph,
    pub lint: LintReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseDiff {
    pub stale: bool,
    pub policy_changed: bool,
    pub current_policy_digest: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// Changed graph-level definition fields, separate from deliverable ids.
    pub graph_changes: Vec<String>,
}

/// Inline schema used by MCP; all nested wire structs reject unknown keys too.
pub fn workflow_schema() -> Value {
    fn object(properties: Value, required: &[&str]) -> Value {
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    fn array(items: Value) -> Value {
        json!({"type":"array","items":items,"maxItems":5000})
    }
    fn expansion(items: Value) -> Value {
        object(
            json!({"items":array(items),"cardinality":{"type":"integer","minimum":0,"maximum":5000},"source":object(json!({"uri":{"type":"string"},"revision":{"type":"string"},"max_items":{"type":"integer","minimum":0,"maximum":5000}}), &["uri","revision","max_items"])}),
            &["items"],
        )
    }
    let text = json!({"type":"string","minLength":1,"maxLength":2048});
    let id = json!({"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"});
    let outcome =
        json!({"type":"string","enum":["planned","succeeded","failed","cancelled","skipped"]});
    let estimate = object(
        json!({"hours":{"type":"number","minimum":0,"maximum":crate::plan::MAX_HOURS},"source":text,"confidence":{"type":"number","minimum":0,"maximum":1}}),
        &["hours", "source", "confidence"],
    );
    let estimates: serde_json::Map<String, Value> = [
        "pr",
        "check",
        "join",
        "approval_wait",
        "approval",
        "merge",
        "producer",
        "receipt",
        "artifact",
        "acceptance",
    ]
    .into_iter()
    .map(|key| (key.into(), estimate.clone()))
    .collect();
    let promotion = object(
        json!({"id":id,"from":text,"to":text,"depends_on":array(id.clone()),"checks":expansion(object(json!({"id":id,"workflow":text}), &["id","workflow"])),"approvals":expansion(object(json!({"id":id,"environment":text}), &["id","environment"]))}),
        &["id", "from", "to"],
    );
    let producer = object(
        json!({"id":id,"workflow":text,"promotion":id,"trigger":object(json!({"matched":{"type":"boolean"},"evidence":text,"paths":array(text.clone())}), &["matched","evidence","paths"]),"attempts":expansion(object(json!({"id":id,"outcome":outcome}), &["id","outcome"])),"artifacts":expansion(object(json!({"id":id,"path":text}), &["id","path"]))}),
        &["id", "workflow", "promotion", "attempts", "artifacts"],
    );
    let repo = object(
        json!({"id":id,"repository":text,"allowed_edges":array(object(json!({"from":text,"to":text}), &["from","to"])),"promotions":expansion(promotion),"producers":expansion(producer)}),
        &["id", "repository", "allowed_edges", "promotions"],
    );
    object(
        json!({"schema_version":{"type":"integer","const":SCHEMA_VERSION},"repositories":expansion(repo),"estimates":object(Value::Object(estimates), &[]),"acceptance":object(json!({"id":id,"dependencies":expansion(object(json!({"repository":id,"promotion":id,"producer":id,"artifact":id}), &["repository","promotion"]))}), &["id","dependencies"]),"history":{"type":"array","maxItems":50000,"items":object(json!({"node_id":text,"duration_hours":{"type":"number","minimum":0,"maximum":crate::plan::MAX_HOURS},"outcome":outcome,"evidence":text,"confidence":{"type":"number","minimum":0,"maximum":1}}), &["node_id","duration_hours","outcome","evidence","confidence"])}}),
        &["schema_version", "repositories", "estimates", "acceptance"],
    )
}

fn invalid(reason: impl Into<String>) -> PlannerError {
    PlannerError::InvalidGraph {
        reason: reason.into(),
    }
}

fn valid_text(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 2048 && !s.chars().any(char::is_control)
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

struct Validator {
    findings: Vec<LintFinding>,
}
impl Validator {
    fn require(&mut self, condition: bool, code: &str, message: impl Into<String>) {
        if !condition {
            self.findings.push(LintFinding {
                code: code.into(),
                severity: Severity::Error,
                message: message.into(),
                ids: Vec::new(),
                path: None,
            });
        }
    }
    fn text(&mut self, s: &str, path: &str) {
        self.require(
            valid_text(s),
            "INVALID_RELEASE_VALUE",
            format!(
                "{path}: expected nonempty text of at most 2048 bytes without control characters"
            ),
        );
    }
    fn ids<'a>(&mut self, ids: impl Iterator<Item = &'a str>, path: &str) {
        let mut seen = BTreeSet::new();
        for id in ids {
            self.require(
                valid_id(id),
                "INVALID_RELEASE_ID",
                format!("{path}: invalid id '{id}' (use 1..64 ASCII letters, digits, '-' or '_')"),
            );
            self.require(
                seen.insert(id),
                "DUPLICATE_RELEASE_ID",
                format!("{path}: duplicate id '{id}'"),
            );
        }
    }
    fn expansion<T>(&mut self, e: &Expansion<T>, path: &str) {
        self.require(
            e.cardinality.is_some() || e.source.is_some(),
            "HIDDEN_CARDINALITY",
            format!("{path}: declare cardinality or a bounded source"),
        );
        if let Some(count) = e.cardinality {
            self.require(
                count == e.items.len(),
                "CARDINALITY_MISMATCH",
                format!("{path}: declared {count}, expanded {}", e.items.len()),
            );
        }
        if let Some(source) = &e.source {
            self.text(&source.uri, path);
            self.text(&source.revision, path);
            self.require(
                source.max_items <= crate::plan::MAX_DELIVERABLES
                    && e.items.len() <= source.max_items,
                "UNBOUNDED_CARDINALITY",
                format!(
                    "{path}: source max_items must bound its items and be <= {}",
                    crate::plan::MAX_DELIVERABLES
                ),
            );
        }
        self.require(
            e.items.len() <= crate::plan::MAX_DELIVERABLES,
            "UNBOUNDED_CARDINALITY",
            format!("{path}: at most {} items", crate::plan::MAX_DELIVERABLES),
        );
    }
}

/// Report missing cardinality and malformed policy before expanding any nodes.
pub fn lint_workflow(w: &ReleaseWorkflow) -> LintReport {
    let mut v = Validator {
        findings: Vec::new(),
    };
    v.require(
        w.schema_version == SCHEMA_VERSION,
        "RELEASE_SCHEMA_VERSION",
        format!(
            "unsupported release schema version {}; expected {SCHEMA_VERSION}",
            w.schema_version
        ),
    );
    v.expansion(&w.repositories, "repositories");
    v.require(
        !w.repositories.items.is_empty(),
        "EMPTY_RELEASE",
        "release needs at least one repository",
    );
    v.ids(
        w.repositories.items.iter().map(|r| r.id.as_str()),
        "repositories",
    );
    v.ids(std::iter::once(w.acceptance.id.as_str()), "acceptance");
    v.expansion(&w.acceptance.dependencies, "acceptance.dependencies");
    v.require(
        !w.acceptance.dependencies.items.is_empty(),
        "EMPTY_ACCEPTANCE",
        "central acceptance needs explicit dependencies",
    );
    for (stage, e) in &w.estimates {
        v.text(&e.source, "estimate.source");
        v.require(
            e.hours.is_finite() && e.hours >= 0.0 && e.hours <= crate::plan::MAX_HOURS,
            "INVALID_RELEASE_ESTIMATE",
            format!("{stage:?}: hours out of range"),
        );
        v.require(
            e.confidence.is_finite() && (0.0..=1.0).contains(&e.confidence),
            "INVALID_RELEASE_ESTIMATE",
            format!("{stage:?}: confidence must be in [0, 1]"),
        );
    }
    for r in &w.repositories.items {
        v.text(&r.repository, "repository");
        v.expansion(&r.promotions, &format!("{}.promotions", r.id));
        v.ids(
            r.promotions.items.iter().map(|p| p.id.as_str()),
            "promotions",
        );
        v.expansion(&r.producers, &format!("{}.producers", r.id));
        v.ids(r.producers.items.iter().map(|p| p.id.as_str()), "producers");
        v.require(
            r.allowed_edges.len() <= crate::plan::MAX_DELIVERABLES,
            "UNBOUNDED_CARDINALITY",
            "too many allowed promotion edges",
        );
        for edge in &r.allowed_edges {
            v.text(&edge.from, "allowed_edges.from");
            v.text(&edge.to, "allowed_edges.to");
        }
        for p in &r.promotions.items {
            v.require(
                r.allowed_edges
                    .iter()
                    .any(|e| e.from == p.from && e.to == p.to),
                "PROMOTION_NOT_ALLOWED",
                format!(
                    "{}/{}: {} -> {} is not an allowed edge",
                    r.id, p.id, p.from, p.to
                ),
            );
            v.require(
                p.from != p.to,
                "INVALID_PROMOTION",
                format!(
                    "{}/{}: promotion must change branch/environment",
                    r.id, p.id
                ),
            );
            v.text(&p.from, "promotion.from");
            v.text(&p.to, "promotion.to");
            v.ids(
                p.depends_on.iter().map(String::as_str),
                "promotion.depends_on",
            );
            v.require(
                p.depends_on.len() <= crate::plan::MAX_DELIVERABLES,
                "UNBOUNDED_CARDINALITY",
                "too many promotion prerequisites",
            );
            for dep in &p.depends_on {
                v.require(
                    r.promotions
                        .items
                        .iter()
                        .any(|q| &q.id == dep && q.to == p.from),
                    "INVALID_PROMOTION_CHAIN",
                    format!(
                        "{}/{}: prerequisite {dep} must promote into {}",
                        r.id, p.id, p.from
                    ),
                );
            }
            v.expansion(&p.checks, "promotion.checks");
            v.ids(p.checks.items.iter().map(|c| c.id.as_str()), "checks");
            for c in &p.checks.items {
                v.text(&c.workflow, "check.workflow");
            }
            v.expansion(&p.approvals, "promotion.approvals");
            v.ids(p.approvals.items.iter().map(|a| a.id.as_str()), "approvals");
            for a in &p.approvals.items {
                v.text(&a.environment, "approval.environment");
            }
        }
        for p in &r.producers.items {
            v.text(&p.workflow, "producer.workflow");
            v.require(
                r.promotions.items.iter().any(|q| q.id == p.promotion),
                "UNKNOWN_PROMOTION",
                format!(
                    "{}/{}: unknown producer promotion {}",
                    r.id, p.id, p.promotion
                ),
            );
            v.expansion(&p.attempts, "producer.attempts");
            v.ids(p.attempts.items.iter().map(|a| a.id.as_str()), "attempts");
            v.expansion(&p.artifacts, "producer.artifacts");
            v.ids(p.artifacts.items.iter().map(|a| a.id.as_str()), "artifacts");
            v.require(
                !p.artifacts.items.is_empty(),
                "MISSING_RELEASE_ARTIFACT",
                format!("{}/{} must retain at least one artifact", r.id, p.id),
            );
            for a in &p.artifacts.items {
                v.text(&a.path, "artifact.path");
            }
            let active = p.trigger.as_ref().is_none_or(|t| t.matched);
            if let Some(t) = &p.trigger {
                v.text(&t.evidence, "trigger.evidence");
                v.require(
                    !t.paths.is_empty() && t.paths.len() <= crate::plan::MAX_DELIVERABLES,
                    "INVALID_TRIGGER",
                    "change trigger needs a bounded, nonempty path list",
                );
                for path in &t.paths {
                    v.text(path, "trigger.paths");
                }
            }
            v.require(
                if active {
                    !p.attempts.items.is_empty()
                } else {
                    p.attempts.items.is_empty()
                },
                "INVALID_PROOF_ATTEMPTS",
                "active producer needs attempts; a change-skipped campaign declares zero attempts",
            );
            for (index, a) in p.attempts.items.iter().enumerate() {
                let last = index + 1 == p.attempts.items.len();
                v.require(if last { matches!(a.outcome, RunOutcome::Succeeded | RunOutcome::Planned) } else { matches!(a.outcome, RunOutcome::Failed | RunOutcome::Cancelled) }, "UNRESOLVED_PROOF_FAILURE", format!("{}/{}: retries must follow failed/cancelled attempts and end with one succeeded/planned attempt", r.id, p.id));
            }
        }
    }
    v.require(
        w.history.len() <= crate::plan::MAX_DELIVERABLES * 10,
        "UNBOUNDED_HISTORY",
        "at most 50000 history samples",
    );
    for h in &w.history {
        v.text(&h.node_id, "history.node_id");
        v.text(&h.evidence, "history.evidence");
        v.require(
            h.duration_hours.is_finite()
                && h.duration_hours >= 0.0
                && h.duration_hours <= crate::plan::MAX_HOURS
                && h.confidence.is_finite()
                && (0.0..=1.0).contains(&h.confidence),
            "INVALID_RELEASE_ESTIMATE",
            "invalid retained duration/confidence",
        );
        v.require(
            h.outcome != RunOutcome::Planned,
            "INVALID_HISTORY",
            "retained run history cannot contain planned outcomes",
        );
    }
    LintReport {
        clean: v.findings.is_empty(),
        findings: v.findings,
    }
}

fn digest(w: &ReleaseWorkflow) -> Result<String, PlannerError> {
    let mut policy = serde_json::to_value(w).map_err(|e| invalid(e.to_string()))?;
    // History changes estimates, but is not a workflow/promotion policy change.
    if let Some(object) = policy.as_object_mut() {
        object.remove("history");
    }
    Ok(
        Sha256::digest(serde_json::to_vec(&policy).map_err(|e| invalid(e.to_string()))?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

#[derive(Clone, Copy)]
enum Resource {
    Agent,
    Owner,
    Runner,
    Wait,
}
impl Resource {
    fn name(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Owner => "owner",
            Self::Runner => "external_runner",
            Self::Wait => "elapsed",
        }
    }
}

struct Compiler<'a> {
    workflow: &'a ReleaseWorkflow,
    digest: String,
    nodes: BTreeMap<String, Deliverable>,
    history: BTreeMap<&'a str, Vec<&'a RetainedRun>>,
}
impl Compiler<'_> {
    fn node(
        &mut self,
        id: String,
        stage: Stage,
        resource: Resource,
        deps: Vec<String>,
        detail: Value,
    ) -> Result<String, PlannerError> {
        if self.nodes.len() >= crate::plan::MAX_DELIVERABLES {
            return Err(invalid(
                "TOO_MANY_DELIVERABLES: expanded release exceeds 5000 nodes",
            ));
        }
        let fallback = self.workflow.estimates.get(&stage).ok_or_else(|| {
            invalid(format!(
                "MISSING_RELEASE_ESTIMATE: no fallback for {stage:?}"
            ))
        })?;
        let history: Vec<_> = self
            .history
            .get(id.as_str())
            .into_iter()
            .flat_map(|runs| runs.iter().copied())
            .filter(|h| h.outcome != RunOutcome::Skipped)
            .collect();
        let mut times: Vec<_> = history.iter().map(|h| h.duration_hours).collect();
        times.sort_by(f32::total_cmp);
        let likely = if times.is_empty() {
            fallback.hours
        } else {
            times[(times.len() - 1) / 2]
        };
        let estimate = Estimate {
            optimistic: times.first().copied().unwrap_or(likely),
            likely,
            pessimistic: times.last().copied().unwrap_or(likely),
        };
        let provenance = if history.is_empty() {
            json!({"kind":"declared", "source":fallback.source, "confidence":fallback.confidence})
        } else {
            json!({"kind":"retained_run_history", "method":"lower_median", "confidence":history.iter().map(|h| h.confidence).fold(1.0_f32, f32::min), "samples":history})
        };
        let elapsed = matches!(resource, Resource::Runner | Resource::Wait);
        let mut metadata = json!({"artifact":format!("Retained {stage:?} evidence for {id}"), "owner":resource.name(),
            "resources":{resource.name():1}, "release":{"schema_version":SCHEMA_VERSION,"policy_digest":self.digest,"stage":stage,"detail":detail,"estimate":provenance}});
        if matches!(
            resource,
            Resource::Owner | Resource::Runner | Resource::Wait
        ) {
            metadata["kind"] = json!("manual");
        }
        if matches!(resource, Resource::Wait) {
            metadata["wall_clock_only"] = json!(true);
        }
        let node = Deliverable {
            id: id.clone(),
            owned_files: vec![OwnedFile::Path(
                format!(".cpm-planner/release-receipts/{id}.json").into(),
            )],
            prerequisites: deps
                .into_iter()
                .map(|prereq| Prerequisite::Edge {
                    consumes: Some(format!(
                        "Evidence receipt {prereq}.json required by {stage:?} artifact {id}"
                    )),
                    id: prereq,
                    kind: None,
                    lag_hours: None,
                })
                .collect(),
            estimated_effort_hours: elapsed.then_some(0.0),
            duration_hours: elapsed.then_some(likely),
            estimate: (!elapsed).then_some(estimate),
            metadata,
            milestone: matches!(stage, Stage::Acceptance),
            earning_rule: None,
        };
        if self.nodes.insert(id.clone(), node).is_some() {
            return Err(invalid(format!("DUPLICATE_RELEASE_ID: generated {id}")));
        }
        Ok(id)
    }
}

fn promotion_id(repo: &str, promotion: &str, suffix: &str) -> String {
    format!("release/{repo}/promotion/{promotion}/{suffix}")
}
fn producer_id(repo: &str, producer: &str, suffix: &str) -> String {
    format!("release/{repo}/producer/{producer}/{suffix}")
}

/// Expand policy into a linted graph; never submit, baseline, accept, or execute it.
pub fn compile(w: &ReleaseWorkflow) -> Result<ReleaseCompilation, PlannerError> {
    if serde_json::to_vec(w)
        .map_err(|e| invalid(e.to_string()))?
        .len() as u64
        > MAX_INPUT_BYTES
    {
        return Err(invalid(
            "RELEASE_INPUT_TOO_LARGE: release workflow exceeds 8 MiB",
        ));
    }
    let lint = lint_workflow(w);
    if !lint.clean {
        return Err(invalid(
            lint.findings
                .iter()
                .map(|f| format!("{}: {}", f.code, f.message))
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let policy_digest = digest(w)?;
    let mut history: BTreeMap<&str, Vec<&RetainedRun>> = BTreeMap::new();
    for run in &w.history {
        history.entry(&run.node_id).or_default().push(run);
    }
    let mut c = Compiler {
        workflow: w,
        digest: policy_digest.clone(),
        nodes: BTreeMap::new(),
        history,
    };
    for r in &w.repositories.items {
        for p in &r.promotions.items {
            let detail = json!({"repository":r.repository,"from":p.from,"to":p.to});
            let pr = c.node(
                promotion_id(&r.id, &p.id, "pr"),
                Stage::Pr,
                Resource::Agent,
                p.depends_on
                    .iter()
                    .map(|id| promotion_id(&r.id, id, "merge"))
                    .collect(),
                detail.clone(),
            )?;
            let mut checks = Vec::new();
            for check in &p.checks.items {
                checks.push(c.node(
                    promotion_id(&r.id, &p.id, &format!("check/{}", check.id)),
                    Stage::Check,
                    Resource::Runner,
                    vec![pr.clone()],
                    json!({"workflow":check.workflow,"promotion":detail}),
                )?);
            }
            let join = c.node(
                promotion_id(&r.id, &p.id, "checks-joined"),
                Stage::Join,
                Resource::Agent,
                if checks.is_empty() { vec![pr] } else { checks },
                detail.clone(),
            )?;
            let mut approvals = Vec::new();
            for approval in &p.approvals.items {
                let detail = json!({"environment":approval.environment,"promotion":detail});
                let wait = c.node(
                    promotion_id(&r.id, &p.id, &format!("approval/{}/wait", approval.id)),
                    Stage::ApprovalWait,
                    Resource::Wait,
                    vec![join.clone()],
                    detail.clone(),
                )?;
                approvals.push(c.node(
                    promotion_id(&r.id, &p.id, &format!("approval/{}", approval.id)),
                    Stage::Approval,
                    Resource::Owner,
                    vec![wait],
                    detail,
                )?);
            }
            c.node(
                promotion_id(&r.id, &p.id, "merge"),
                Stage::Merge,
                Resource::Owner,
                if approvals.is_empty() {
                    vec![join]
                } else {
                    approvals
                },
                detail,
            )?;
        }
        for p in &r.producers.items {
            let mut last = promotion_id(&r.id, &p.promotion, "merge");
            let detail =
                json!({"repository":r.repository,"workflow":p.workflow,"trigger":p.trigger});
            if p.trigger.as_ref().is_some_and(|t| !t.matched) {
                last = c.node(
                    producer_id(&r.id, &p.id, "skipped"),
                    Stage::Receipt,
                    Resource::Agent,
                    vec![last],
                    json!({"outcome":"skipped","campaign":detail}),
                )?;
            } else {
                for attempt in &p.attempts.items {
                    let run = c.node(
                        producer_id(&r.id, &p.id, &format!("attempt/{}/run", attempt.id)),
                        Stage::Producer,
                        Resource::Runner,
                        vec![last],
                        json!({"attempt":attempt,"campaign":detail}),
                    )?;
                    last = c.node(
                        producer_id(&r.id, &p.id, &format!("attempt/{}/receipt", attempt.id)),
                        Stage::Receipt,
                        Resource::Agent,
                        vec![run],
                        json!({"attempt":attempt,"campaign":detail}),
                    )?;
                }
            }
            for a in &p.artifacts.items {
                c.node(
                    producer_id(&r.id, &p.id, &format!("artifact/{}", a.id)),
                    Stage::Artifact,
                    Resource::Agent,
                    vec![last.clone()],
                    json!({"artifact":a,"campaign":detail}),
                )?;
            }
        }
    }
    let mut deps = BTreeSet::new();
    for d in &w.acceptance.dependencies.items {
        let repo = w
            .repositories
            .items
            .iter()
            .find(|r| r.id == d.repository)
            .ok_or_else(|| invalid(format!("UNKNOWN_REPOSITORY: {}", d.repository)))?;
        let id = match (&d.producer, &d.artifact) {
            (None, None) => promotion_id(&repo.id, &d.promotion, "merge"),
            (Some(producer), Some(artifact)) => {
                if !repo
                    .producers
                    .items
                    .iter()
                    .any(|p| p.id == *producer && p.promotion == d.promotion)
                {
                    return Err(invalid(
                        "INVALID_ACCEPTANCE_DEPENDENCY: producer belongs to a different promotion or does not exist",
                    ));
                }
                producer_id(&repo.id, producer, &format!("artifact/{artifact}"))
            }
            _ => {
                return Err(invalid(
                    "INVALID_ACCEPTANCE_DEPENDENCY: producer and artifact must be provided together",
                ));
            }
        };
        if !c.nodes.contains_key(&id) {
            return Err(invalid(format!("UNKNOWN_ACCEPTANCE_DEPENDENCY: {id}")));
        }
        if !deps.insert(id) {
            return Err(invalid("DUPLICATE_ACCEPTANCE_DEPENDENCY"));
        }
    }
    c.node(
        format!("release/acceptance/{}", w.acceptance.id),
        Stage::Acceptance,
        Resource::Owner,
        deps.into_iter().collect(),
        json!({"acceptance":w.acceptance.id}),
    )?;
    for h in &w.history {
        if !c.nodes.contains_key(&h.node_id) {
            return Err(invalid(format!("UNKNOWN_HISTORY_NODE: {}", h.node_id)));
        }
    }
    let graph = PlanGraph {
        deliverables: c.nodes.into_values().collect(),
        max_chained_dispatch: None,
    };
    if serde_json::to_vec_pretty(&graph)
        .map_err(|e| invalid(e.to_string()))?
        .len() as u64
        > crate::project::MAX_PLAN_FILE_BYTES
    {
        return Err(invalid(
            "RELEASE_GRAPH_TOO_LARGE: generated plan file exceeds 8 MiB",
        ));
    }
    let lint = crate::lint::lint(&graph);
    if !lint.clean {
        return Err(invalid(format!(
            "generated graph is not lint clean: {}",
            lint.findings
                .iter()
                .filter(|f| f.severity != Severity::Info)
                .map(|f| format!("{}: {}", f.code, f.message))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    crate::planner::validate_graph(&graph)?;
    Ok(ReleaseCompilation {
        schema_version: SCHEMA_VERSION,
        policy_digest,
        graph,
        lint,
    })
}

/// Compare a retained plan definition with the current workflow/promotion policy.
/// Recompilation is read-only; callers apply changes through normal plan.revise/sync.
pub fn diff(w: &ReleaseWorkflow, previous: &PlanGraph) -> Result<ReleaseDiff, PlannerError> {
    if previous.deliverables.len() > crate::plan::MAX_DELIVERABLES {
        return Err(invalid(
            "TOO_MANY_DELIVERABLES: previous plan exceeds 5000 nodes",
        ));
    }
    let current = compile(w)?;
    let old: BTreeMap<_, _> = previous
        .deliverables
        .iter()
        .map(|d| (d.id.clone(), d))
        .collect();
    if old.len() != previous.deliverables.len() {
        return Err(invalid(
            "DUPLICATE_ID: previous plan contains duplicate ids",
        ));
    }
    let new: BTreeMap<_, _> = current
        .graph
        .deliverables
        .iter()
        .map(|d| (d.id.clone(), d))
        .collect();
    let policy_changed = previous.deliverables.iter().any(|d| {
        d.metadata
            .pointer("/release/policy_digest")
            .and_then(Value::as_str)
            != Some(&current.policy_digest)
    });
    let added = new
        .keys()
        .filter(|id| !old.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let removed = old
        .keys()
        .filter(|id| !new.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let changed = new
        .iter()
        .filter(|(id, d)| old.get(*id).is_some_and(|before| *before != **d))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let graph_changes = if previous.max_chained_dispatch != current.graph.max_chained_dispatch {
        vec!["max_chained_dispatch".to_string()]
    } else {
        Vec::new()
    };
    Ok(ReleaseDiff {
        stale: policy_changed
            || !added.is_empty()
            || !removed.is_empty()
            || !changed.is_empty()
            || !graph_changes.is_empty(),
        policy_changed,
        current_policy_digest: current.policy_digest,
        added,
        removed,
        changed,
        graph_changes,
    })
}
