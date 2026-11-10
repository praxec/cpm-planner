//! Verification evidence bound to its governed identity (version 1).
//!
//! The contract is `docs/evidence.md`. In short:
//!
//! - an evidence entry is a plain [`Evidence::Note`] string (stored and
//!   validated exactly as before) or an [`EvidenceRecord`]: a [`Scope`], a
//!   summary and an [`Identity`] (tree, dependency lock, toolchain, test
//!   contract, environment);
//! - the planner stores a record as a [`StoredRecord`], adding who recorded
//!   it, when, through which call, at which plan revision and the
//!   [`definition_digest`] of the deliverable at that time;
//! - [`record_status`] and [`evidence_status`] judge stored evidence against
//!   a target identity and the deliverable's current definition: `current`,
//!   `stale` (naming each changed field), `unknown` or `unbound`;
//! - [`scope_requirement`] says which scopes may close a deliverable of each
//!   kind, and the planner enforces the closing rules on `mark_status` and
//!   `accept`.
//!
//! Everything here is pure and deterministic, except the
//! [`BasicCpmPlanner::evidence_status`] and [`BasicCpmPlanner::evidence`]
//! reads at the bottom. A verdict is reported, never acted on.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::plan::{Deliverable, PlanGraph, PlanId, PlannerError};
use crate::planner::BasicCpmPlanner;

/// The only `schema_version` of a record and of an identity.
pub const SCHEMA_VERSION: u32 = 1;

/// Longest identity field value, in characters.
pub const MAX_IDENTITY_FIELD_CHARS: usize = 256;

/// Longest record `summary`, in characters (the note limit).
pub const MAX_SUMMARY_CHARS: usize = crate::plan::MAX_EVIDENCE_CHARS;

/// Longest record `ref`, in characters.
pub const MAX_REF_CHARS: usize = 2048;

/// Longest record `party`, in characters.
pub const MAX_PARTY_CHARS: usize = 256;

/// `metadata` key declaring the scope a deliverable must be closed with.
pub const EVIDENCE_SCOPE_KEY: &str = "evidence_scope";

/// `metadata` key naming the direct prerequisite whose candidate a gate
/// verifies.
pub const CANDIDATE_KEY: &str = "candidate";

/// Field names the server adds to a stored record; a caller may not send them.
const SERVER_FIELDS: [&str; 5] = [
    "recorded_at",
    "recorded_by",
    "via",
    "plan_revision",
    "definition_digest",
];

fn invalid(reason: impl Into<String>) -> PlannerError {
    PlannerError::InvalidEvidence {
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// What a record proves. The vocabulary is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The deliverable's own cheap checks passed.
    Focused,
    /// A full regression, end-to-end or CI run passed on one candidate.
    Gate,
    /// A named owner approved the artifact.
    ManualApproval,
    /// A party outside the team accepted a contract or artifact.
    ExternalSignoff,
    /// A change was merged into a protected branch.
    Merge,
    /// An artifact was published.
    Publish,
    /// A published artifact was activated in an environment.
    Activation,
}

impl Scope {
    /// Every scope, in vocabulary order.
    pub const ALL: [Scope; 7] = [
        Scope::Focused,
        Scope::Gate,
        Scope::ManualApproval,
        Scope::ExternalSignoff,
        Scope::Merge,
        Scope::Publish,
        Scope::Activation,
    ];

    /// The wire name, e.g. `"manual_approval"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Focused => "focused",
            Scope::Gate => "gate",
            Scope::ManualApproval => "manual_approval",
            Scope::ExternalSignoff => "external_signoff",
            Scope::Merge => "merge",
            Scope::Publish => "publish",
            Scope::Activation => "activation",
        }
    }

    /// The scope with wire name `name`, if any.
    pub fn parse(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A field a record is judged on: the five identity fields, then the
/// deliverable's definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityField {
    Tree,
    DependencyLock,
    Toolchain,
    TestContract,
    Environment,
    Definition,
}

impl IdentityField {
    /// The five caller-supplied identity fields, in comparison order.
    pub const IDENTITY: [IdentityField; 5] = [
        IdentityField::Tree,
        IdentityField::DependencyLock,
        IdentityField::Toolchain,
        IdentityField::TestContract,
        IdentityField::Environment,
    ];

    /// The wire name, e.g. `"dependency_lock"`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityField::Tree => "tree",
            IdentityField::DependencyLock => "dependency_lock",
            IdentityField::Toolchain => "toolchain",
            IdentityField::TestContract => "test_contract",
            IdentityField::Environment => "environment",
            IdentityField::Definition => "definition",
        }
    }
}

impl std::fmt::Display for IdentityField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Identity and records
// ---------------------------------------------------------------------------

/// An evidence identity, schema version 1: the inputs that govern whether a
/// verification result still holds. Also the shape of a target identity, in
/// which every field is optional. Values are opaque and compared byte for
/// byte; the planner never computes or normalises them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_lock: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_contract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

impl Default for Identity {
    fn default() -> Self {
        Self::new()
    }
}

impl Identity {
    /// An empty version 1 identity.
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tree: None,
            dependency_lock: None,
            toolchain: None,
            test_contract: None,
            environment: None,
        }
    }

    /// Set one identity field. [`IdentityField::Definition`] is not an
    /// identity field and is ignored.
    pub fn with(mut self, field: IdentityField, value: impl Into<String>) -> Self {
        let value = Some(value.into());
        match field {
            IdentityField::Tree => self.tree = value,
            IdentityField::DependencyLock => self.dependency_lock = value,
            IdentityField::Toolchain => self.toolchain = value,
            IdentityField::TestContract => self.test_contract = value,
            IdentityField::Environment => self.environment = value,
            IdentityField::Definition => {}
        }
        self
    }

    /// The value of one identity field (`None` for `Definition`).
    pub fn get(&self, field: IdentityField) -> Option<&str> {
        match field {
            IdentityField::Tree => self.tree.as_deref(),
            IdentityField::DependencyLock => self.dependency_lock.as_deref(),
            IdentityField::Toolchain => self.toolchain.as_deref(),
            IdentityField::TestContract => self.test_contract.as_deref(),
            IdentityField::Environment => self.environment.as_deref(),
            IdentityField::Definition => None,
        }
    }

    /// True when no identity field is set.
    pub fn is_empty(&self) -> bool {
        IdentityField::IDENTITY
            .iter()
            .all(|f| self.get(*f).is_none())
    }

    /// Check the schema version and every value's bounds
    /// (`INVALID_EVIDENCE`).
    pub fn validate(&self) -> Result<(), PlannerError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "identity schema_version must be {SCHEMA_VERSION}, got {}",
                self.schema_version
            )));
        }
        for field in IdentityField::IDENTITY {
            if let Some(value) = self.get(field) {
                let chars = value.chars().count();
                if chars == 0 || chars > MAX_IDENTITY_FIELD_CHARS {
                    return Err(invalid(format!(
                        "identity.{field} must be 1 to {MAX_IDENTITY_FIELD_CHARS} characters, got \
                         {chars}"
                    )));
                }
                if value.chars().any(char::is_control) {
                    return Err(invalid(format!(
                        "identity.{field} must not contain control characters"
                    )));
                }
                if value.trim() != value {
                    return Err(invalid(format!(
                        "identity.{field} must not have leading or trailing whitespace"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Parse a JSON identity (for example a read call's `evidence_target`),
    /// mapping every failure to `INVALID_EVIDENCE`.
    pub fn from_json(value: serde_json::Value) -> Result<Self, PlannerError> {
        let identity: Identity =
            serde_json::from_value(value).map_err(|e| invalid(format!("identity: {e}")))?;
        identity.validate()?;
        Ok(identity)
    }
}

/// The identity fields a record of `scope` must carry, and whether it must
/// carry an identity at all.
fn required_identity(scope: Scope) -> (bool, &'static [IdentityField]) {
    match scope {
        Scope::Focused | Scope::Gate => (true, &IdentityField::IDENTITY),
        Scope::Merge | Scope::Publish => (true, &[IdentityField::Tree]),
        Scope::Activation => (true, &[IdentityField::Tree, IdentityField::Environment]),
        Scope::ManualApproval | Scope::ExternalSignoff => (false, &[]),
    }
}

/// An evidence record as a caller sends it, schema version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub schema_version: u32,
    pub scope: Scope,
    pub summary: String,
    /// The record's `ref`: a run, pull request, commit or document.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Who outside the team signed off; only for `external_signoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
}

impl EvidenceRecord {
    /// A version 1 record of `scope` with `summary` and nothing else.
    pub fn new(scope: Scope, summary: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            scope,
            summary: summary.into(),
            reference: None,
            party: None,
            identity: None,
        }
    }

    /// Set the `ref`.
    pub fn with_ref(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    /// Set the `party` (for `external_signoff`).
    pub fn with_party(mut self, party: impl Into<String>) -> Self {
        self.party = Some(party.into());
        self
    }

    /// Set the identity.
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Check the record against the version 1 rules (`INVALID_EVIDENCE`).
    pub fn validate(&self) -> Result<(), PlannerError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "record schema_version must be {SCHEMA_VERSION}, got {}",
                self.schema_version
            )));
        }
        let bounded = |name: &str, text: &str, min: usize, max: usize| {
            let chars = text.chars().count();
            if chars < min || chars > max {
                Err(invalid(format!(
                    "{name} must be {min} to {max} characters, got {chars}"
                )))
            } else {
                Ok(())
            }
        };
        bounded("summary", &self.summary, 1, MAX_SUMMARY_CHARS)?;
        if let Some(r) = &self.reference {
            bounded("ref", r, 0, MAX_REF_CHARS)?;
        }
        match (&self.party, self.scope) {
            (Some(p), Scope::ExternalSignoff) => bounded("party", p, 1, MAX_PARTY_CHARS)?,
            (None, Scope::ExternalSignoff) => {
                return Err(invalid("an external_signoff record requires party"));
            }
            (Some(_), scope) => {
                return Err(invalid(format!(
                    "party is only accepted on external_signoff, not {scope}"
                )));
            }
            (None, _) => {}
        }
        let (needs_identity, fields) = required_identity(self.scope);
        match &self.identity {
            Some(identity) => {
                identity.validate()?;
                for field in fields {
                    if identity.get(*field).is_none() {
                        return Err(invalid(format!(
                            "a {} record requires identity.{field}",
                            self.scope
                        )));
                    }
                }
            }
            None if needs_identity => {
                return Err(invalid(format!(
                    "a {} record requires an identity",
                    self.scope
                )));
            }
            None => {}
        }
        Ok(())
    }

    /// Parse and validate a JSON record, mapping every failure (unknown
    /// field or version, a server-added field, bad bounds) to
    /// `INVALID_EVIDENCE`.
    pub fn from_json(value: serde_json::Value) -> Result<Self, PlannerError> {
        if let Some(object) = value.as_object() {
            for field in SERVER_FIELDS {
                if object.contains_key(field) {
                    return Err(invalid(format!(
                        "{field} is added by the server and may not be sent"
                    )));
                }
            }
        }
        let record: EvidenceRecord =
            serde_json::from_value(value).map_err(|e| invalid(format!("record: {e}")))?;
        record.validate()?;
        Ok(record)
    }
}

/// One piece of evidence on `plan.mark_status` or `plan.accept`: a plain
/// note (exactly today's string) or a record. Serialises untagged, so a
/// note is a JSON string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Evidence {
    Note(String),
    Record(EvidenceRecord),
}

impl Evidence {
    /// The note text, when this is a note.
    pub fn as_note(&self) -> Option<&str> {
        match self {
            Evidence::Note(note) => Some(note),
            Evidence::Record(_) => None,
        }
    }

    /// The record, when this is one.
    pub fn as_record(&self) -> Option<&EvidenceRecord> {
        match self {
            Evidence::Note(_) => None,
            Evidence::Record(record) => Some(record),
        }
    }

    /// Parse a wire value: a JSON string is a note, an object is a record
    /// ([`EvidenceRecord::from_json`]); anything else is `INVALID_EVIDENCE`.
    pub fn from_json(value: serde_json::Value) -> Result<Self, PlannerError> {
        match value {
            serde_json::Value::String(note) => Ok(Evidence::Note(note)),
            object @ serde_json::Value::Object(_) => {
                EvidenceRecord::from_json(object).map(Evidence::Record)
            }
            other => Err(invalid(format!(
                "evidence must be a string or a record object, got {other}"
            ))),
        }
    }
}

impl From<String> for Evidence {
    fn from(note: String) -> Self {
        Evidence::Note(note)
    }
}

impl From<&str> for Evidence {
    fn from(note: &str) -> Self {
        Evidence::Note(note.to_string())
    }
}

impl From<&String> for Evidence {
    fn from(note: &String) -> Self {
        Evidence::Note(note.clone())
    }
}

impl From<EvidenceRecord> for Evidence {
    fn from(record: EvidenceRecord) -> Self {
        Evidence::Record(record)
    }
}

impl std::fmt::Display for Evidence {
    /// A note verbatim; a record as its summary.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Evidence::Note(note) => f.write_str(note),
            Evidence::Record(record) => f.write_str(&record.summary),
        }
    }
}

/// Which call stored a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    MarkStatus,
    Accept,
}

/// A record as stored: the caller's fields plus the ones the server adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub schema_version: u32,
    pub scope: Scope,
    pub summary: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    pub recorded_at: DateTime<Utc>,
    pub recorded_by: String,
    pub via: Via,
    pub plan_revision: u32,
    pub definition_digest: String,
}

impl StoredRecord {
    /// Stamp `record` with its server-added fields.
    pub fn new(
        record: EvidenceRecord,
        recorded_at: DateTime<Utc>,
        recorded_by: impl Into<String>,
        via: Via,
        plan_revision: u32,
        definition_digest: impl Into<String>,
    ) -> Self {
        let EvidenceRecord {
            schema_version,
            scope,
            summary,
            reference,
            party,
            identity,
        } = record;
        Self {
            schema_version,
            scope,
            summary,
            reference,
            party,
            identity,
            recorded_at,
            recorded_by: recorded_by.into(),
            via,
            plan_revision,
            definition_digest: definition_digest.into(),
        }
    }
}

/// One stored evidence entry: a note (as stored before records existed) or
/// a stored record. Serialises untagged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredEvidence {
    Note(String),
    Record(Box<StoredRecord>),
}

impl StoredEvidence {
    /// The text shown where a plain string is expected: a note verbatim, a
    /// record's summary.
    pub fn display_text(&self) -> &str {
        match self {
            StoredEvidence::Note(note) => note,
            StoredEvidence::Record(record) => &record.summary,
        }
    }

    /// The stored record, when this is one.
    pub fn as_record(&self) -> Option<&StoredRecord> {
        match self {
            StoredEvidence::Note(_) => None,
            StoredEvidence::Record(record) => Some(record),
        }
    }
}

impl From<String> for StoredEvidence {
    fn from(note: String) -> Self {
        StoredEvidence::Note(note)
    }
}

impl From<&str> for StoredEvidence {
    fn from(note: &str) -> Self {
        StoredEvidence::Note(note.to_string())
    }
}

// ---------------------------------------------------------------------------
// Definition digest
// ---------------------------------------------------------------------------

/// `value` with every object's keys in sorted order, so the serialised form
/// is canonical whatever map ordering serde_json was built with.
fn sorted_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), sorted_keys(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sorted_keys).collect())
        }
        other => other.clone(),
    }
}

/// `sha256:` plus the lowercase hex SHA-256 of the deliverable's canonical
/// definition: the form `plan.revise` compares to decide that a deliverable
/// changed, serialised with sorted keys.
pub fn definition_digest(d: &Deliverable) -> String {
    let canonical = sorted_keys(&crate::planner::canonical_deliverable(d));
    let bytes =
        serde_json::to_vec(&canonical).expect("INVARIANT: a serde_json::Value always serialises");
    let hash = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(7 + hash.len() * 2);
    hex.push_str("sha256:");
    for byte in hash {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

// ---------------------------------------------------------------------------
// Kinds and scopes
// ---------------------------------------------------------------------------

/// A deliverable's kind for evidence purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableKind {
    /// Anything that is not a gate, manual or a milestone.
    Work,
    /// `metadata.kind == "gate"`.
    Gate,
    /// `metadata.kind == "manual"`.
    Manual,
    /// A milestone that is neither a gate nor manual.
    Milestone,
}

/// The evidence kind of `d`: gate and manual first, then milestone, else
/// work.
pub fn kind_of(d: &Deliverable) -> DeliverableKind {
    match d.metadata.get("kind").and_then(serde_json::Value::as_str) {
        Some(crate::waves::GATE_KIND) => DeliverableKind::Gate,
        Some("manual") => DeliverableKind::Manual,
        _ if d.is_milestone() => DeliverableKind::Milestone,
        _ => DeliverableKind::Work,
    }
}

/// The scopes a deliverable of `kind` may declare in
/// `metadata.evidence_scope`.
pub fn declarable_scopes(kind: DeliverableKind) -> &'static [Scope] {
    match kind {
        DeliverableKind::Work => &[Scope::Focused],
        DeliverableKind::Gate => &[Scope::Gate, Scope::Merge, Scope::Publish, Scope::Activation],
        DeliverableKind::Manual => &[
            Scope::ManualApproval,
            Scope::ExternalSignoff,
            Scope::Merge,
            Scope::Publish,
            Scope::Activation,
        ],
        DeliverableKind::Milestone => &[],
    }
}

/// Where a required scope came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeSource {
    /// `metadata.evidence_scope`.
    Declared,
    /// The kind's default.
    Default,
}

/// Which evidence may close a deliverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeRequirement {
    pub kind: DeliverableKind,
    /// The declared or default scope; `None` for an undeclared manual
    /// deliverable or a milestone.
    pub required: Option<Scope>,
    pub source: Option<ScopeSource>,
    /// The scopes that may close it; `None` means any scope.
    pub allowed: Option<Vec<Scope>>,
}

impl ScopeRequirement {
    /// True when a record of `scope` may close the deliverable.
    pub fn allows(&self, scope: Scope) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(&scope))
    }

    /// True when the scope was declared in the plan, so closing needs it.
    pub fn is_declared(&self) -> bool {
        self.source == Some(ScopeSource::Declared)
    }
}

/// The scope requirement of `d`. An `evidence_scope` that is not a string,
/// not a scope, or not one the kind may declare is refused with
/// `EVIDENCE_SCOPE_MISMATCH` (the lint reports it as
/// `EVIDENCE_SCOPE_INVALID`).
pub fn scope_requirement(d: &Deliverable) -> Result<ScopeRequirement, PlannerError> {
    let kind = kind_of(d);
    let declared = match d.metadata.get(EVIDENCE_SCOPE_KEY) {
        None => None,
        Some(value) => {
            let scope = value
                .as_str()
                .and_then(Scope::parse)
                .filter(|s| declarable_scopes(kind).contains(s));
            match scope {
                Some(scope) => Some(scope),
                None => {
                    let may: Vec<&str> =
                        declarable_scopes(kind).iter().map(|s| s.as_str()).collect();
                    return Err(PlannerError::EvidenceScopeMismatch {
                        deliverable_id: d.id.clone(),
                        reason: format!(
                            "metadata.evidence_scope {value} is not a scope a {} deliverable may \
                             declare (one of: {}); fix the plan",
                            kind_name(kind),
                            if may.is_empty() {
                                "none".to_string()
                            } else {
                                may.join(", ")
                            }
                        ),
                    });
                }
            }
        }
    };
    let (required, source) = match (declared, kind) {
        (Some(scope), _) => (Some(scope), Some(ScopeSource::Declared)),
        (None, DeliverableKind::Work) => (Some(Scope::Focused), Some(ScopeSource::Default)),
        (None, DeliverableKind::Gate) => (Some(Scope::Gate), Some(ScopeSource::Default)),
        (None, DeliverableKind::Manual | DeliverableKind::Milestone) => (None, None),
    };
    let allowed = match (kind, required) {
        (DeliverableKind::Work, _) => Some(vec![Scope::Focused, Scope::Gate]),
        (_, Some(scope)) => Some(vec![scope]),
        (_, None) => None,
    };
    Ok(ScopeRequirement {
        kind,
        required,
        source,
        allowed,
    })
}

fn kind_name(kind: DeliverableKind) -> &'static str {
    match kind {
        DeliverableKind::Work => "work",
        DeliverableKind::Gate => "gate",
        DeliverableKind::Manual => "manual",
        DeliverableKind::Milestone => "milestone",
    }
}

/// The gate's declared candidate, when `d` is a gate whose
/// `metadata.candidate` names one of its direct prerequisites.
pub fn candidate_of(d: &Deliverable) -> Option<&str> {
    if kind_of(d) != DeliverableKind::Gate {
        return None;
    }
    let candidate = d.metadata.get(CANDIDATE_KEY)?.as_str()?;
    d.prerequisites
        .iter()
        .any(|p| p.id() == candidate)
        .then_some(candidate)
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// Whether evidence still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every field the record holds matches, and so does the definition.
    Current,
    /// At least one field changed.
    Stale,
    /// Nothing changed, but at least one field could not be compared.
    Unknown,
    /// A plain note: no scope, no identity.
    Unbound,
    /// Deliverable level only: no evidence at all.
    None,
}

/// One changed field: the value recorded and the target's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: IdentityField,
    pub recorded: String,
    pub target: String,
}

/// A target identity and where it came from (`"argument"` or
/// `"candidate:<id>"`).
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub identity: &'a Identity,
    pub source: &'a str,
}

/// The status of one stored evidence entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStatus {
    /// Position in the deliverable's evidence list, from 0.
    pub index: usize,
    pub scope: Option<Scope>,
    pub verdict: Verdict,
    pub target_source: Option<String>,
    pub changed: Vec<FieldChange>,
    pub unknown: Vec<IdentityField>,
    pub recorded_at: Option<DateTime<Utc>>,
    pub recorded_by: Option<String>,
}

/// Judge one stored entry against `target` and the deliverable's current
/// `current_digest`. A note is `unbound`.
pub fn record_status(
    index: usize,
    entry: &StoredEvidence,
    current_digest: &str,
    target: Option<Target<'_>>,
) -> RecordStatus {
    let record = match entry {
        StoredEvidence::Note(_) => {
            return RecordStatus {
                index,
                scope: None,
                verdict: Verdict::Unbound,
                target_source: None,
                changed: Vec::new(),
                unknown: Vec::new(),
                recorded_at: None,
                recorded_by: None,
            };
        }
        StoredEvidence::Record(record) => record,
    };
    let mut changed = Vec::new();
    let mut unknown = Vec::new();
    if let Some(identity) = &record.identity {
        for field in IdentityField::IDENTITY {
            let Some(recorded) = identity.get(field) else {
                continue;
            };
            match target.and_then(|t| t.identity.get(field)) {
                Some(value) if value == recorded => {}
                Some(value) => changed.push(FieldChange {
                    field,
                    recorded: recorded.to_string(),
                    target: value.to_string(),
                }),
                None => unknown.push(field),
            }
        }
    }
    if record.definition_digest != current_digest {
        changed.push(FieldChange {
            field: IdentityField::Definition,
            recorded: record.definition_digest.clone(),
            target: current_digest.to_string(),
        });
    }
    let verdict = if !changed.is_empty() {
        Verdict::Stale
    } else if !unknown.is_empty() {
        Verdict::Unknown
    } else {
        Verdict::Current
    };
    RecordStatus {
        index,
        scope: Some(record.scope),
        verdict,
        target_source: target.map(|t| t.source.to_string()),
        changed,
        unknown,
        recorded_at: Some(record.recorded_at),
        recorded_by: Some(record.recorded_by.clone()),
    }
}

/// The status of a deliverable's evidence: its governing record's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverableEvidenceStatus {
    pub deliverable_id: String,
    pub required_scope: Option<Scope>,
    pub scope_source: Option<ScopeSource>,
    /// Number of entries of both kinds.
    pub entries: usize,
    pub verdict: Verdict,
    pub record: Option<RecordStatus>,
}

/// The newest identity held by any record in `entries`.
fn newest_identity(entries: &[StoredEvidence]) -> Option<&Identity> {
    entries
        .iter()
        .rev()
        .filter_map(StoredEvidence::as_record)
        .find_map(|r| r.identity.as_ref())
}

/// The index of the governing record: the newest record whose scope may
/// close the deliverable, else the newest record of any scope.
fn governing_index(
    entries: &[StoredEvidence],
    requirement: Option<&ScopeRequirement>,
) -> Option<usize> {
    let records = || {
        entries
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(i, e)| e.as_record().map(|r| (i, r)))
    };
    records()
        .find(|(_, r)| requirement.is_some_and(|req| req.allows(r.scope)))
        .or_else(|| records().next())
        .map(|(i, _)| i)
}

/// The evidence status of `deliverable_id` in `graph`, given every
/// deliverable's stored evidence and an optional plan-wide `target`
/// (the read call's `evidence_target`). Target resolution: the argument,
/// else (for a gate with a valid `metadata.candidate`) the candidate's
/// newest identity, else none. `None` when the deliverable is not in the
/// graph. A malformed `evidence_scope` reads as no required scope.
pub fn evidence_status(
    graph: &PlanGraph,
    evidence: &HashMap<String, Vec<StoredEvidence>>,
    deliverable_id: &str,
    target: Option<&Identity>,
) -> Option<DeliverableEvidenceStatus> {
    let d = graph.deliverables.iter().find(|d| d.id == deliverable_id)?;
    let entries: &[StoredEvidence] = evidence.get(deliverable_id).map_or(&[], Vec::as_slice);
    let requirement = scope_requirement(d).ok();
    let base = DeliverableEvidenceStatus {
        deliverable_id: d.id.clone(),
        required_scope: requirement.as_ref().and_then(|r| r.required),
        scope_source: requirement.as_ref().and_then(|r| r.source),
        entries: entries.len(),
        verdict: Verdict::None,
        record: None,
    };
    if entries.is_empty() {
        return Some(base);
    }
    let Some(index) = governing_index(entries, requirement.as_ref()) else {
        return Some(DeliverableEvidenceStatus {
            verdict: Verdict::Unbound,
            ..base
        });
    };
    let candidate = candidate_of(d).and_then(|c| {
        newest_identity(evidence.get(c).map_or(&[], Vec::as_slice))
            .map(|identity| (identity, format!("candidate:{c}")))
    });
    let resolved = match (target, &candidate) {
        (Some(identity), _) => Some(Target {
            identity,
            source: "argument",
        }),
        (None, Some((identity, source))) => Some(Target {
            identity,
            source: source.as_str(),
        }),
        (None, None) => None,
    };
    let status = record_status(index, &entries[index], &definition_digest(d), resolved);
    Some(DeliverableEvidenceStatus {
        verdict: status.verdict,
        record: Some(status),
        ..base
    })
}

/// [`evidence_status`] of every deliverable, in graph declaration order.
pub fn plan_evidence_status(
    graph: &PlanGraph,
    evidence: &HashMap<String, Vec<StoredEvidence>>,
    target: Option<&Identity>,
) -> Vec<DeliverableEvidenceStatus> {
    graph
        .deliverables
        .iter()
        .filter_map(|d| evidence_status(graph, evidence, &d.id, target))
        .collect()
}

// ---------------------------------------------------------------------------
// Closing rules
// ---------------------------------------------------------------------------

/// What [`check_call`] needs to know about one `mark_status` or `accept`.
pub(crate) struct CallCheck<'a> {
    pub(crate) deliverable: &'a Deliverable,
    /// The evidence the call carries.
    pub(crate) incoming: Option<&'a Evidence>,
    /// True when the call moves the deliverable to `Complete` from another
    /// status (an idempotent complete is not closing).
    pub(crate) closing: bool,
}

/// Evidence the closing rules may need to read, loaded lazily by the caller
/// only when [`needs_stored`] says so.
pub(crate) struct StoredContext<'a> {
    pub(crate) own: &'a [StoredEvidence],
    pub(crate) candidate: Option<(&'a str, &'a [StoredEvidence])>,
}

/// True when [`check_call`] must see stored evidence (a closing call on a
/// deliverable with a declared scope or a candidate).
pub(crate) fn needs_stored(call: &CallCheck<'_>) -> bool {
    call.closing
        && (call.deliverable.metadata.get(EVIDENCE_SCOPE_KEY).is_some()
            || candidate_of(call.deliverable).is_some())
}

/// Apply the closing rules of the contract to one call. Rule 1 (a record
/// whose scope may not close the deliverable is refused) applies to every
/// call; rules 2 (a declared scope must be met) and 3 (a gate's record must
/// be current against its candidate) only to a closing call. `stored` must be
/// given when [`needs_stored`] is true.
pub(crate) fn check_call(
    call: &CallCheck<'_>,
    stored: Option<&StoredContext<'_>>,
) -> Result<(), PlannerError> {
    let d = call.deliverable;
    let incoming = call.incoming.and_then(Evidence::as_record);
    let declared_or_record = d.metadata.get(EVIDENCE_SCOPE_KEY).is_some() || incoming.is_some();
    if !declared_or_record && !(call.closing && candidate_of(d).is_some()) {
        return Ok(());
    }
    let requirement = scope_requirement(d)?;

    // Rule 1.
    if let Some(record) = incoming
        && !requirement.allows(record.scope)
    {
        let allowed: Vec<&str> = requirement
            .allowed
            .iter()
            .flatten()
            .map(|s| s.as_str())
            .collect();
        return Err(PlannerError::EvidenceScopeMismatch {
            deliverable_id: d.id.clone(),
            reason: format!(
                "a {} record may not close a {} deliverable; allowed: {}",
                record.scope,
                kind_name(requirement.kind),
                allowed.join(", ")
            ),
        });
    }
    if !call.closing {
        return Ok(());
    }
    let digest = definition_digest(d);
    let own = stored.map_or(&[][..], |s| s.own);

    // The record that closes: the incoming one if it is allowed, else the
    // newest stored allowed record at the current definition.
    let closing_record: Option<StoredRecord> = match incoming {
        Some(record) => Some(StoredRecord::new(
            record.clone(),
            DateTime::<Utc>::UNIX_EPOCH,
            "",
            Via::MarkStatus,
            0,
            digest.clone(),
        )),
        None => own
            .iter()
            .rev()
            .filter_map(StoredEvidence::as_record)
            .find(|r| requirement.allows(r.scope) && r.definition_digest == digest)
            .cloned(),
    };

    // Rule 2.
    if requirement.is_declared() && closing_record.is_none() {
        return Err(PlannerError::EvidenceRequired {
            deliverable_id: d.id.clone(),
            scope: requirement
                .required
                .map_or_else(String::new, |s| s.as_str().to_string()),
        });
    }

    // Rule 3.
    if let Some((candidate, candidate_entries)) = stored.and_then(|s| s.candidate)
        && let Some(identity) = newest_identity(candidate_entries)
    {
        let Some(record) = closing_record else {
            return Err(PlannerError::EvidenceRequired {
                deliverable_id: d.id.clone(),
                scope: requirement
                    .required
                    .map_or_else(String::new, |s| s.as_str().to_string()),
            });
        };
        let source = format!("candidate:{candidate}");
        let status = record_status(
            0,
            &StoredEvidence::Record(Box::new(record)),
            &digest,
            Some(Target {
                identity,
                source: &source,
            }),
        );
        if status.verdict != Verdict::Current {
            let mut fields: Vec<String> = status
                .changed
                .iter()
                .map(|c| c.field.as_str().to_string())
                .collect();
            fields.extend(status.unknown.iter().map(|f| format!("{f} (unknown)")));
            return Err(PlannerError::EvidenceStale {
                deliverable_id: d.id.clone(),
                candidate: candidate.to_string(),
                changed: fields,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

impl BasicCpmPlanner {
    /// The stored evidence of every deliverable of `plan_id` that has any,
    /// in append order. Read-only; works on any plan.
    pub async fn evidence(
        &self,
        plan_id: &PlanId,
    ) -> Result<HashMap<String, Vec<StoredEvidence>>, PlannerError> {
        self.store().read_tx(|tx| {
            if crate::plan_store::load_plan_state(tx, plan_id)?.is_none() {
                return Err(PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                });
            }
            crate::ev_store::load_evidence(tx, plan_id)
        })
    }

    /// The evidence status of every deliverable of `plan_id`, in graph
    /// order, judged against the optional plan-wide `target` (see
    /// [`evidence_status`]). Read-only and deterministic for a given stored
    /// state; a target is validated first (`INVALID_EVIDENCE`).
    pub async fn evidence_status(
        &self,
        plan_id: &PlanId,
        target: Option<&Identity>,
    ) -> Result<Vec<DeliverableEvidenceStatus>, PlannerError> {
        if let Some(target) = target {
            target.validate()?;
        }
        self.store().read_tx(|tx| {
            let state = crate::plan_store::load_plan_state(tx, plan_id)?.ok_or_else(|| {
                PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                }
            })?;
            let evidence = crate::ev_store::load_evidence(tx, plan_id)?;
            Ok(plan_evidence_status(&state.graph, &evidence, target))
        })
    }
}

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Apply the closing rules to a `mark_status` or `accept` call on
/// `deliverable_id` inside its transaction, and stamp its evidence for
/// storage. `closing` is true when the call moves the deliverable to
/// `Complete` from another status. Returns the entry to append, if any.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_evidence(
    tx: &rusqlite::Transaction<'_>,
    plan_id: &PlanId,
    graph: &PlanGraph,
    deliverable_id: &str,
    evidence: Option<&Evidence>,
    closing: bool,
    recorded_by: &str,
    via: Via,
    now: DateTime<Utc>,
) -> Result<Option<crate::ev_store::EvidenceAppend>, PlannerError> {
    let Some(d) = graph.deliverables.iter().find(|d| d.id == deliverable_id) else {
        return Err(PlannerError::DeliverableNotFound {
            plan_id: plan_id.0.clone(),
            deliverable_id: deliverable_id.to_string(),
        });
    };
    let call = CallCheck {
        deliverable: d,
        incoming: evidence,
        closing,
    };
    if needs_stored(&call) {
        let own = crate::ev_store::load_deliverable_evidence(tx, plan_id, deliverable_id)?;
        let candidate = candidate_of(d)
            .map(|c| crate::ev_store::load_deliverable_evidence(tx, plan_id, c).map(|e| (c, e)))
            .transpose()?;
        let context = StoredContext {
            own: &own,
            candidate: candidate.as_ref().map(|(c, e)| (*c, e.as_slice())),
        };
        check_call(&call, Some(&context))?;
    } else {
        check_call(&call, None)?;
    }
    let entry = match evidence {
        None => return Ok(None),
        Some(Evidence::Note(note)) => StoredEvidence::Note(note.clone()),
        Some(Evidence::Record(record)) => StoredEvidence::Record(Box::new(StoredRecord::new(
            record.clone(),
            now,
            recorded_by,
            via,
            crate::ev_store::head_revision(tx, plan_id)?,
            definition_digest(d),
        ))),
    };
    Ok(Some(crate::ev_store::EvidenceAppend {
        entry,
        drop_oldest_at_cap: via == Via::Accept,
    }))
}
