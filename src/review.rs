//! `plan.review`: deterministic lint, one batched Jev judgment call, and
//! mechanically verified proposals (P6, #26).
//!
//! [`review`] runs, in order:
//!
//! 1. **Request checks.** `max_questions` must be in `1..=`[`MAX_QUESTIONS`]
//!    (else [`PlannerError::InvalidGraph`]); `capacities` is checked by the
//!    resource scheduler when the base plan is simulated.
//! 2. **Lint** ([`crate::lint::lint`]), always reported.
//! 3. **Lint short-circuit.** Any lint `error` finding returns
//!    `invalid_graph` whatever the judge (configured, missing or
//!    misconfigured); the judge is never called.
//! 4. **Judge availability.** [`Judge::NotConfigured`] returns
//!    `review_unavailable` with [`NO_KEY_REASON`]; [`Judge::ConfigError`]
//!    returns `review_unavailable` with the caller's reason (cut to
//!    [`MAX_ERROR_MESSAGE_CHARS`]). No questions are built.
//! 5. **Base simulation** ([`simulate`], leveled when `capacities` is
//!    given). Its makespan is the "before" of every proposal.
//! 6. **Candidate generation** (deterministic, see below), then
//!    **truncation** to the question cap, round-robin across kinds.
//! 7. **One** [`JudgmentModel::decide`] call with every selected question
//!    and one shared state. No candidates means no call. A judge failure
//!    returns `review_unavailable` with `"<class>: <safe message>"`.
//! 8. **Findings** from the answers, **proposals** from the confident ones,
//!    each **verified** by [`apply_edits`] + [`simulate`] and dropped unless
//!    it lints clean and saves time; then **ranking**.
//!
//! # Candidates
//!
//! `L` is a deliverable's scheduled length from [`compute_cpm`]; a
//! "critical" deliverable has zero float.
//!
//! | kind | question | who is asked about | rank within kind |
//! |---|---|---|---|
//! | `crash_option` | Choice `add_capacity` / `fast_track` / `reduce_scope` / `none` | critical, non-milestone deliverables with `L > 0` | `L` desc, id |
//! | `false_dependency` | Noul | every prerequisite edge | both ends critical, then one end, then neither; then the prerequisite's `L` desc; then ids |
//! | `interface_split` | Noul | edges whose ends are both critical and whose kind is not `interface` | prerequisite's `L` desc, ids |
//! | `split_candidate` | Score, [`SPLIT_LEVELS`] (5 levels) | non-milestones with `L >= `[`SPLIT_MIN_HOURS`] or `L >= `[`SPLIT_MEDIAN_FACTOR`]` × median L` (median over non-milestones with `L > 0`) | `L` desc, id |
//! | `missing_dependency` | Noul | unordered pairs (neither reaches the other) with a heuristic score `> 0`, top [`MISSING_DEPENDENCY_TOP_K`] | score desc, ids |
//!
//! The missing-dependency score adds [`MENTION_WEIGHT`] when either
//! deliverable's `metadata.description` contains the other's id as a token
//! (whitespace-separated, outer punctuation trimmed); [`SHARED_DIR_WEIGHT`]
//! when they own files in the same directory, or [`NESTED_DIR_WEIGHT`] when
//! one file's directory is a proper ancestor of the other's (compared
//! component-wise, so `src/ap` is not a prefix of `src/api`; top-level
//! files and directories deeper than [`MAX_DIR_DEPTH`] components carry no
//! signal); and [`SHARED_OWNER_WEIGHT`] when their `metadata.owner` strings
//! are equal. An owner or same-directory group of more than
//! [`MAX_SIGNAL_GROUP`] deliverables carries no signal, nor does an ancestor
//! directory whose subtree (its own entries plus all descendant
//! directories') has more than [`MAX_SIGNAL_GROUP`] entries. Directory and
//! owner pairs are each generated in sorted order and stop at
//! [`MAX_CANDIDATE_PAIRS`], so the work is bounded on any graph.
//!
//! The selected lists are interleaved round-robin in the table's order
//! (crash, false dependency, interface split, split, missing dependency)
//! until the cap is reached, so truncation keeps every kind represented;
//! `truncated` is true when candidates were left out. Question ids are
//! `q00`, `q01`, … in selection order; each question's `instructions` is
//! `{ kind, ids, question }`. The shared state lists the plan makespan, the
//! critical path, and every deliverable a question names (sorted by id),
//! with free text cut to [`MAX_STATE_TEXT_CHARS`] chars.
//!
//! `prompt_hash` is the sha256 (lower-case hex) of the canonical request
//! JSON `{ "questions": …, "state": … }` with object keys sorted at every
//! level and no whitespace.
//!
//! # Findings
//!
//! A finding is reported when its probability is at least
//! [`FINDING_MIN_PROBABILITY`]:
//!
//! - Noul kinds: the answer's `noul` (probability the statement is true).
//! - `split_candidate`: the probability mass on levels
//!   [`SPLIT_LIKELY_LEVEL`] and above.
//! - `crash_option`: the probability of the chosen label, unless it is
//!   `none`.
//!
//! Answers are validated defensively; each of these yields nothing (no
//! finding, no proposal, nothing echoed): a missing answer, an answer of the
//! wrong type, a non-finite or out-of-`[0, 1]` probability or confidence, a
//! Choice whose `choice` or probability keys are not labels the question
//! asked, and a Score outside `0..=4` or with probability keys other than
//! `"0"`..`"4"`. The provider's model id is cut to [`MAX_MODEL_CHARS`]. Findings sort by kind, then
//! ids.
//!
//! # Proposals
//!
//! - `false_dependency` with probability `>= `[`FALSE_DEPENDENCY_PROPOSAL_P`]
//!   → `RemoveEdge`, cost `0`.
//! - `crash_option` with answer confidence
//!   `>= `[`CRASH_PROPOSAL_CONFIDENCE`]:
//!   - `add_capacity` → `SetDuration` to [`CRASH_LENGTH_FACTOR`]` × L`,
//!     effort unchanged (more hands shorten calendar time, not the work;
//!     `duration_hours` replaces effort as the scheduled length in CPM,
//!     leveling and simulate, while effort stays the cost basis). Cost
//!     [`ADD_CAPACITY_COST_FACTOR`]` × L` hours of coordination overhead.
//!   - `reduce_scope` → `SetEffort` to [`CRASH_LENGTH_FACTOR`]` × L`
//!     (`SetDuration` instead when the deliverable has `duration_hours`,
//!     since effort would not change its scheduled length). Cost `0`.
//!   - `fast_track` and `none` propose nothing.
//! - Splits, interface splits and missing dependencies are findings only:
//!   the calling agent designs those edits.
//!
//! Each proposal is applied with [`apply_edits`] and simulated with the
//! request's capacities. It is dropped when either fails, when the edited
//! graph has a lint warning or error not present (same code and ids) in the
//! base lint, or when `hours_saved` is below [`MIN_HOURS_SAVED`].
//! `hours_saved = makespan(before) − makespan(after)`, using the leveled
//! makespan when capacities are given and the CPM makespan otherwise.
//! Edit hours, `hours_saved` and `cost` are rounded to `1 / `[`HOURS_SCALE`]
//! hours, computed in f64. `score = hours_saved / max(cost, 1)`; proposals sort by score
//! descending, then id.
//!
//! Output is a pure function of the graph, the request and the judge's
//! answers, and never contains NaN or infinity.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::edits::{GraphEdit, apply_edits};
use crate::lint::{LintReport, Severity, lint};
use crate::llm::{Answer, JudgmentModel, MAX_ERROR_MESSAGE_CHARS, Question, Usage};
use crate::plan::{Deliverable, PlanGraph, PlannerError, PrerequisiteKind};
use crate::resource_schedule::ScheduleRequest;
use crate::schedule::compute_cpm;
use crate::simulate::{SimulateRequest, SimulationResult, simulate};
use crate::task::Task;

/// Most questions one review may send (and the default cap).
pub const MAX_QUESTIONS: u16 = 64;
/// `review_unavailable` reason when no OpenRouter key is configured.
pub const NO_KEY_REASON: &str = "no OpenRouter key configured";
/// Least probability for a finding to be reported.
pub const FINDING_MIN_PROBABILITY: f32 = 0.5;
/// Least false-dependency probability that proposes removing the edge.
pub const FALSE_DEPENDENCY_PROPOSAL_P: f32 = 0.8;
/// Least answer confidence for a crash option to be proposed.
pub const CRASH_PROPOSAL_CONFIDENCE: f32 = 0.6;
/// A crashed deliverable's new scheduled length, as a fraction of `L`.
pub const CRASH_LENGTH_FACTOR: f32 = 0.7;
/// Effort an `add_capacity` crash adds, as a fraction of `L`.
pub const ADD_CAPACITY_COST_FACTOR: f32 = 0.3;
/// A deliverable of at least this many hours is a split candidate.
pub const SPLIT_MIN_HOURS: f32 = 8.0;
/// A deliverable of at least this multiple of the median is a split candidate.
pub const SPLIT_MEDIAN_FACTOR: f32 = 2.0;
/// The split rubric, level 0 (no) to 4 (clearly).
pub const SPLIT_LEVELS: [&str; 5] = [
    "not splittable: one indivisible unit of work",
    "splitting would gain little",
    "could be split, with modest benefit",
    "likely worth splitting into parts that can proceed in parallel",
    "clearly should be split into parallel parts",
];
/// Split levels from this index up count toward the finding probability.
pub const SPLIT_LIKELY_LEVEL: usize = 3;
/// Most missing-dependency pairs asked about.
pub const MISSING_DEPENDENCY_TOP_K: usize = 16;
/// Missing-dependency score for a description mentioning the other id.
pub const MENTION_WEIGHT: u32 = 3;
/// Missing-dependency score for owning files in the same directory.
pub const SHARED_DIR_WEIGHT: u32 = 2;
/// Missing-dependency score for owning files where one's directory is a
/// proper ancestor of the other's.
pub const NESTED_DIR_WEIGHT: u32 = 1;
/// Most directory-signal (and, separately, owner-signal) pairs generated
/// before scoring.
pub const MAX_CANDIDATE_PAIRS: usize = 10_000;
/// Directories deeper than this many components carry no signal (bounds
/// the ancestor walk).
pub const MAX_DIR_DEPTH: usize = 32;
/// Missing-dependency score for the same `metadata.owner`.
pub const SHARED_OWNER_WEIGHT: u32 = 1;
/// Directory / owner groups larger than this carry no pair signal.
pub const MAX_SIGNAL_GROUP: usize = 32;
/// Longest free-text field copied into the judge state, in chars.
pub const MAX_STATE_TEXT_CHARS: usize = 500;
/// Smallest saving that keeps a proposal, in hours.
pub const MIN_HOURS_SAVED: f32 = 0.01;
/// Proposal hours are rounded to `1 / HOURS_SCALE` (0.01 h).
pub const HOURS_SCALE: f64 = 100.0;
/// Longest provider-reported model id kept in the report, in chars.
pub const MAX_MODEL_CHARS: usize = 64;

/// Crash technique labels, as asked.
const ADD_CAPACITY: &str = "add_capacity";
const FAST_TRACK: &str = "fast_track";
const REDUCE_SCOPE: &str = "reduce_scope";
const NONE: &str = "none";

/// `Ok` when `max_questions` is within `1..=MAX_QUESTIONS`.
pub fn check_max_questions(max_questions: u16) -> Result<(), String> {
    if (1..=MAX_QUESTIONS).contains(&max_questions) {
        Ok(())
    } else {
        Err(format!(
            "max_questions must be between 1 and {MAX_QUESTIONS}; got {max_questions}"
        ))
    }
}

/// What to review beyond the graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    /// Resource capacities; when present, makespans are leveled.
    #[serde(default)]
    pub capacities: Option<ScheduleRequest>,
    /// Question cap, `1..=64`; default 64.
    #[serde(default)]
    pub max_questions: Option<u16>,
}

/// The judge a review may use.
#[derive(Clone, Copy)]
pub enum Judge<'a> {
    /// A configured judgment model.
    Model(&'a dyn JudgmentModel),
    /// No key configured: [`NO_KEY_REASON`].
    NotConfigured,
    /// The configuration is unusable; the caller supplies a key-free reason
    /// (e.g. "key file ignored: world-readable").
    ConfigError(&'a str),
}

/// Outcome class of a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    /// Jev answered; findings and proposals are filled.
    Ok,
    /// No judge, or the judge failed; `reason` says why.
    ReviewUnavailable,
    /// Lint found errors; the judge was not called.
    InvalidGraph,
}

/// What a finding or proposal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// An edge the dependent probably does not need.
    FalseDependency,
    /// An unordered pair that probably needs an edge.
    MissingDependency,
    /// A deliverable worth splitting into parallel parts.
    SplitCandidate,
    /// An edge the dependent could satisfy with an early interface.
    InterfaceSplit,
    /// A critical deliverable with a realistic crash technique.
    CrashOption,
}

impl FindingKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::FalseDependency => "false_dependency",
            Self::MissingDependency => "missing_dependency",
            Self::SplitCandidate => "split_candidate",
            Self::InterfaceSplit => "interface_split",
            Self::CrashOption => "crash_option",
        }
    }
}

/// One calibrated judgment worth the agent's attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub kind: FindingKind,
    /// Deliverables involved: `[from, to]` for edges, `[a, b]` (sorted) for
    /// pairs, `[id]` otherwise.
    pub ids: Vec<String>,
    /// In `[0, 1]`; see the module docs for its meaning per kind.
    pub probability: f32,
    pub evidence: String,
}

/// A verified edit batch `plan.fork { edits }` can apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// Stable id, e.g. `false_dependency:a->b`, `crash_option:reduce_scope:a`.
    pub id: String,
    pub kind: FindingKind,
    pub edits: Vec<GraphEdit>,
    pub hours_saved: f32,
    /// Added effort hours.
    pub cost: f32,
    /// `hours_saved / max(cost, 1)`.
    pub score: f32,
    pub rationale: String,
}

/// Everything `plan.review` reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub status: ReviewStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub lint: LintReport,
    pub findings: Vec<ReviewFinding>,
    pub proposals: Vec<Proposal>,
    /// Candidates were left out to respect the question cap.
    pub truncated: bool,
    pub jev_called: bool,
    /// Model id the provider reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Host of the judge endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
    pub question_count: usize,
}

impl ReviewReport {
    fn new(status: ReviewStatus, reason: Option<String>, lint: LintReport) -> Self {
        Self {
            status,
            reason,
            lint,
            findings: Vec::new(),
            proposals: Vec::new(),
            truncated: false,
            jev_called: false,
            model: None,
            endpoint: None,
            usage: None,
            prompt_hash: None,
            question_count: 0,
        }
    }
}

/// Review `graph`. See the module docs for the pipeline and every rule.
///
/// # Errors
///
/// [`PlannerError::InvalidGraph`] for an out-of-range `max_questions`, or
/// when the base plan cannot be simulated (for example a resource with no
/// capacity, or a graph submit would reject that lint did not flag).
pub async fn review(
    graph: &PlanGraph,
    req: &ReviewRequest,
    judge: Judge<'_>,
) -> Result<ReviewReport, PlannerError> {
    let cap = req.max_questions.unwrap_or(MAX_QUESTIONS);
    check_max_questions(cap).map_err(|reason| PlannerError::InvalidGraph { reason })?;
    let lint_report = lint(graph);
    if lint_report
        .findings
        .iter()
        .any(|f| f.severity == Severity::Error)
    {
        return Ok(ReviewReport::new(
            ReviewStatus::InvalidGraph,
            Some("lint reported errors; fix them before review".to_string()),
            lint_report,
        ));
    }

    let model = match judge {
        Judge::Model(model) => model,
        Judge::NotConfigured => {
            return Ok(ReviewReport::new(
                ReviewStatus::ReviewUnavailable,
                Some(NO_KEY_REASON.to_string()),
                lint_report,
            ));
        }
        Judge::ConfigError(reason) => {
            return Ok(ReviewReport::new(
                ReviewStatus::ReviewUnavailable,
                Some(reason.chars().take(MAX_ERROR_MESSAGE_CHARS).collect()),
                lint_report,
            ));
        }
    };

    let sim_req = SimulateRequest {
        schedule: req.capacities.clone(),
        monte_carlo: None,
    };
    let base = simulate(graph, &sim_req)?;
    let base_makespan = makespan(&base);
    let cpm = compute_cpm(graph)?;
    let tasks: HashMap<&str, &Task> = cpm.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let by_id: HashMap<&str, &Deliverable> = graph
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    let ctx = Context {
        graph,
        tasks,
        by_id,
    };

    let lists = [
        ctx.crash_candidates(),
        ctx.false_dependency_candidates(),
        ctx.interface_split_candidates(),
        ctx.split_candidates(),
        ctx.missing_dependency_candidates(),
    ];
    let total: usize = lists.iter().map(Vec::len).sum();
    let selected = round_robin(lists, usize::from(cap));

    let mut report = ReviewReport::new(ReviewStatus::Ok, None, lint_report);
    report.truncated = total > selected.len();
    report.endpoint = model.endpoint_host();
    if selected.is_empty() {
        return Ok(report);
    }

    let questions: BTreeMap<String, Question> = selected
        .iter()
        .enumerate()
        .map(|(i, c)| (question_id(i), c.question.clone()))
        .collect();
    let state = ctx.state(&selected, base_makespan, &cpm.critical_path);
    report.question_count = questions.len();
    report.prompt_hash = Some(prompt_hash(&state, &questions));
    report.jev_called = true;

    let decisions = match model.decide(state, questions).await {
        Ok(decisions) => decisions,
        Err(error) => {
            report.status = ReviewStatus::ReviewUnavailable;
            report.reason = Some(error.to_string());
            return Ok(report);
        }
    };
    report.model = Some(decisions.model.chars().take(MAX_MODEL_CHARS).collect());
    report.usage = decisions.usage.map(sanitize_usage);

    let mut drafts = Vec::new();
    for (i, candidate) in selected.iter().enumerate() {
        let Some(answer) = decisions.answers.get(&question_id(i)) else {
            continue;
        };
        let Some(judged) = interpret(candidate, answer) else {
            continue;
        };
        if judged.probability >= FINDING_MIN_PROBABILITY {
            report.findings.push(ReviewFinding {
                kind: candidate.kind,
                ids: candidate.ids.clone(),
                probability: judged.probability,
                evidence: judged.evidence.clone(),
            });
        }
        if let Some(draft) = ctx.draft(candidate, &judged) {
            drafts.push(draft);
        }
    }
    report
        .findings
        .sort_by(|a, b| (a.kind, &a.ids).cmp(&(b.kind, &b.ids)));

    let base_issues = lint_issues(&report.lint);
    report.proposals = drafts
        .into_iter()
        .filter_map(|d| verify(graph, &sim_req, &base_issues, base_makespan, d))
        .collect();
    report
        .proposals
        .sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    Ok(report)
}

// ---------------------------------------------------------------- candidates

struct Candidate {
    kind: FindingKind,
    ids: Vec<String>,
    question: Question,
}

struct Context<'g> {
    graph: &'g PlanGraph,
    tasks: HashMap<&'g str, &'g Task>,
    by_id: HashMap<&'g str, &'g Deliverable>,
}

impl Context<'_> {
    fn length(&self, id: &str) -> f32 {
        self.tasks.get(id).map_or(0.0, |t| t.effort_hours)
    }

    fn critical(&self, id: &str) -> bool {
        self.tasks.get(id).is_some_and(|t| t.is_critical)
    }

    fn deliverable(&self, id: &str) -> Option<&Deliverable> {
        self.by_id.get(id).copied()
    }

    fn crash_candidates(&self) -> Vec<Candidate> {
        let mut ds: Vec<(&Deliverable, f32)> = self
            .graph
            .deliverables
            .iter()
            .filter(|d| !d.is_milestone() && self.critical(&d.id))
            .map(|d| (d, self.length(&d.id)))
            .filter(|(_, l)| *l > 0.0)
            .collect();
        ds.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        ds.into_iter()
            .map(|(d, l)| {
                let criteria = BTreeMap::from([
                    (
                        ADD_CAPACITY.to_string(),
                        Some(json!(
                            "add a person or agent to shorten it, at extra coordination effort"
                        )),
                    ),
                    (
                        FAST_TRACK.to_string(),
                        Some(json!("overlap it with its prerequisites or dependents")),
                    ),
                    (
                        REDUCE_SCOPE.to_string(),
                        Some(json!("cut or defer part of its scope")),
                    ),
                    (
                        NONE.to_string(),
                        Some(json!("no realistic way to shorten it")),
                    ),
                ]);
                let ids = vec![d.id.clone()];
                Candidate {
                    kind: FindingKind::CrashOption,
                    question: Question::Choice {
                        instructions: instructions(
                            FindingKind::CrashOption,
                            &ids,
                            format!(
                                "'{}' is on the critical path ({} h). Which technique would most \
                                 realistically shorten it?",
                                d.id,
                                fmt_hours(l)
                            ),
                        ),
                        criteria,
                    },
                    ids,
                }
            })
            .collect()
    }

    /// Every distinct `(from, to)` edge with the dependent.
    fn edges(&self) -> Vec<(&str, &Deliverable)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for d in &self.graph.deliverables {
            for p in &d.prerequisites {
                if seen.insert((p.id(), d.id.as_str())) {
                    out.push((p.id(), d));
                }
            }
        }
        out
    }

    fn consumes(to: &Deliverable, from: &str) -> String {
        let stated: Vec<&str> = to
            .prerequisites
            .iter()
            .filter(|p| p.id() == from)
            .filter_map(|p| p.consumes())
            .collect();
        if stated.is_empty() {
            "unstated".to_string()
        } else {
            truncate(&stated.join("; "))
        }
    }

    fn false_dependency_candidates(&self) -> Vec<Candidate> {
        let mut edges: Vec<(u8, f32, &str, &Deliverable)> = self
            .edges()
            .into_iter()
            .map(|(from, to)| {
                let tier = match (self.critical(from), self.critical(&to.id)) {
                    (true, true) => 0,
                    (true, false) | (false, true) => 1,
                    (false, false) => 2,
                };
                (tier, self.length(from), from, to)
            })
            .collect();
        edges.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| (a.2, &a.3.id).cmp(&(b.2, &b.3.id)))
        });
        edges
            .into_iter()
            .map(|(_, _, from, to)| {
                let ids = vec![from.to_string(), to.id.clone()];
                Candidate {
                    kind: FindingKind::FalseDependency,
                    question: Question::Noul {
                        instructions: instructions(
                            FindingKind::FalseDependency,
                            &ids,
                            format!(
                                "'{to}' lists '{from}' as a prerequisite (stated as consuming: \
                                 {consumes}). Is this a false dependency, i.e. can '{to}' be done \
                                 correctly without waiting for '{from}' to finish?",
                                to = to.id,
                                consumes = Self::consumes(to, from),
                            ),
                        ),
                        criteria: Some(BTreeMap::from([
                            (
                                "true".to_string(),
                                json!(format!("'{}' does not need '{from}' to finish", to.id)),
                            ),
                            (
                                "false".to_string(),
                                json!(format!(
                                    "'{}' genuinely needs what '{from}' produces",
                                    to.id
                                )),
                            ),
                        ])),
                    },
                    ids,
                }
            })
            .collect()
    }

    fn interface_split_candidates(&self) -> Vec<Candidate> {
        let mut edges: Vec<(f32, &str, &Deliverable)> = self
            .edges()
            .into_iter()
            .filter(|(from, to)| self.critical(from) && self.critical(&to.id))
            .filter(|(from, to)| {
                !to.prerequisites
                    .iter()
                    .any(|p| p.id() == *from && p.kind() == Some(PrerequisiteKind::Interface))
            })
            .map(|(from, to)| (self.length(from), from, to))
            .collect();
        edges.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| (a.1, &a.2.id).cmp(&(b.1, &b.2.id)))
        });
        edges
            .into_iter()
            .map(|(_, from, to)| {
                let ids = vec![from.to_string(), to.id.clone()];
                Candidate {
                    kind: FindingKind::InterfaceSplit,
                    question: Question::Noul {
                        instructions: instructions(
                            FindingKind::InterfaceSplit,
                            &ids,
                            format!(
                                "Could '{to}' start against an interface or contract that \
                                 '{from}' publishes early (splitting '{from}' into interface and \
                                 implementation), instead of waiting for all of '{from}'?",
                                to = to.id
                            ),
                        ),
                        criteria: None,
                    },
                    ids,
                }
            })
            .collect()
    }

    fn split_candidates(&self) -> Vec<Candidate> {
        let mut lengths: Vec<f32> = self
            .graph
            .deliverables
            .iter()
            .filter(|d| !d.is_milestone())
            .map(|d| self.length(&d.id))
            .filter(|l| *l > 0.0)
            .collect();
        lengths.sort_by(f32::total_cmp);
        let median = match lengths.len() {
            0 => 0.0,
            n if n % 2 == 1 => lengths[n / 2],
            n => (lengths[n / 2 - 1] + lengths[n / 2]) / 2.0,
        };
        let threshold = if median > 0.0 {
            SPLIT_MIN_HOURS.min(SPLIT_MEDIAN_FACTOR * median)
        } else {
            SPLIT_MIN_HOURS
        };
        let mut ds: Vec<(&Deliverable, f32)> = self
            .graph
            .deliverables
            .iter()
            .filter(|d| !d.is_milestone())
            .map(|d| (d, self.length(&d.id)))
            .filter(|(_, l)| *l > 0.0 && *l >= threshold)
            .collect();
        ds.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        ds.into_iter()
            .map(|(d, l)| {
                let ids = vec![d.id.clone()];
                Candidate {
                    kind: FindingKind::SplitCandidate,
                    question: Question::Score {
                        instructions: instructions(
                            FindingKind::SplitCandidate,
                            &ids,
                            format!(
                                "'{}' is scheduled for {} h (plan median {} h). How strongly \
                                 should it be split into smaller deliverables that can proceed in \
                                 parallel?",
                                d.id,
                                fmt_hours(l),
                                fmt_hours(median)
                            ),
                        ),
                        criteria: SPLIT_LEVELS.iter().map(|s| json!(s)).collect(),
                    },
                    ids,
                }
            })
            .collect()
    }

    fn missing_dependency_candidates(&self) -> Vec<Candidate> {
        let ds = &self.graph.deliverables;
        let index: HashMap<&str, usize> = ds
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id.as_str(), i))
            .collect();
        let pair = |a: usize, b: usize| ordered_pair(ds, a, b);

        let mut mentions: BTreeSet<(usize, usize)> = BTreeSet::new();
        for (i, d) in ds.iter().enumerate() {
            let Some(text) = d.metadata.get("description").and_then(Value::as_str) else {
                continue;
            };
            for token in text.split_whitespace() {
                let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if let Some(&j) = index.get(token)
                    && j != i
                {
                    mentions.insert(pair(i, j));
                }
            }
        }

        // Members in deliverable-id order, so the pair cap does not depend
        // on declaration order.
        let mut owners: BTreeMap<&str, BTreeSet<(&str, usize)>> = BTreeMap::new();
        for (i, d) in ds.iter().enumerate() {
            if let Some(owner) = d.metadata.get("owner").and_then(Value::as_str) {
                owners.entry(owner).or_default().insert((d.id.as_str(), i));
            }
        }
        let mut owner_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
        'owners: for members in owners.values().filter(|m| m.len() <= MAX_SIGNAL_GROUP) {
            let members: Vec<usize> = members.iter().map(|(_, i)| *i).collect();
            for (k, &a) in members.iter().enumerate() {
                for &b in &members[k + 1..] {
                    if owner_pairs.len() >= MAX_CANDIDATE_PAIRS {
                        break 'owners;
                    }
                    owner_pairs.insert(pair(a, b));
                }
            }
        }
        let dir_pairs = directory_pairs(ds);

        let mut scores: BTreeMap<(usize, usize), u32> = BTreeMap::new();
        for p in &mentions {
            *scores.entry(*p).or_default() += MENTION_WEIGHT;
        }
        for (p, weight) in &dir_pairs {
            *scores.entry(*p).or_default() += weight;
        }
        for p in &owner_pairs {
            *scores.entry(*p).or_default() += SHARED_OWNER_WEIGHT;
        }
        if scores.is_empty() {
            return Vec::new();
        }
        let reach = crate::graph::reachability(self.graph);
        let ordered = |a: &str, b: &str| {
            reach.get(a).is_some_and(|r| r.contains(b))
                || reach.get(b).is_some_and(|r| r.contains(a))
        };
        let mut ranked: Vec<((usize, usize), u32)> = scores
            .into_iter()
            .filter(|((a, b), _)| !ordered(&ds[*a].id, &ds[*b].id))
            .collect();
        ranked.sort_by(|x, y| {
            y.1.cmp(&x.1)
                .then_with(|| (&ds[x.0.0].id, &ds[x.0.1].id).cmp(&(&ds[y.0.0].id, &ds[y.0.1].id)))
        });
        ranked.truncate(MISSING_DEPENDENCY_TOP_K);
        ranked
            .into_iter()
            .map(|((a, b), _)| {
                let key = (a, b);
                let mut signals = Vec::new();
                if mentions.contains(&key) {
                    signals.push("a description mentions the other");
                }
                match dir_pairs.get(&key) {
                    Some(&SHARED_DIR_WEIGHT) => {
                        signals.push("they own files in the same directory");
                    }
                    Some(_) => signals.push("they own files in nested directories"),
                    None => {}
                }
                if owner_pairs.contains(&key) {
                    signals.push("they share metadata.owner");
                }
                let ids = vec![ds[a].id.clone(), ds[b].id.clone()];
                Candidate {
                    kind: FindingKind::MissingDependency,
                    question: Question::Noul {
                        instructions: instructions(
                            FindingKind::MissingDependency,
                            &ids,
                            format!(
                                "'{}' and '{}' are unordered in the plan, yet {}. Does one of them \
                                 need the other's output first (a missing dependency)?",
                                ids[0],
                                ids[1],
                                signals.join(" and ")
                            ),
                        ),
                        criteria: None,
                    },
                    ids,
                }
            })
            .collect()
    }

    /// The shared state: plan summary plus every deliverable a question
    /// names, sorted by id.
    fn state(&self, selected: &[Candidate], makespan: f32, critical_path: &[String]) -> Value {
        let named: BTreeSet<&str> = selected
            .iter()
            .flat_map(|c| c.ids.iter().map(String::as_str))
            .collect();
        let deliverables: Vec<Value> = named
            .into_iter()
            .filter_map(|id| self.deliverable(id))
            .map(|d| {
                let text = |key: &str| {
                    d.metadata.get(key).map(|v| match v {
                        Value::String(s) => truncate(s),
                        other => truncate(&other.to_string()),
                    })
                };
                let task = self.tasks.get(d.id.as_str());
                json!({
                    "id": d.id,
                    "scheduled_hours": self.length(&d.id),
                    "float_hours": task.map_or(0.0, |t| t.float),
                    "critical": self.critical(&d.id),
                    "milestone": d.is_milestone(),
                    "description": text("description"),
                    "artifact": text("artifact"),
                    "owner": text("owner"),
                    "owned_files": d
                        .owned_files
                        .iter()
                        .map(|f| f.path().to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                    "prerequisites": d
                        .prerequisites
                        .iter()
                        .map(|p| json!({
                            "id": p.id(),
                            "consumes": p.consumes().map(truncate),
                            "kind": p.kind(),
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "task": "Review a software delivery plan scheduled with the critical path \
                     method. Each question names deliverables by id; their details are in \
                     `deliverables`.",
            "makespan_hours": makespan,
            "critical_path": critical_path
                .iter()
                .filter(|id| self.deliverable(id).is_some())
                .collect::<Vec<_>>(),
            "deliverables": deliverables,
        })
    }

    /// The proposal a judged candidate earns, if any (unverified).
    fn draft(&self, candidate: &Candidate, judged: &Judged) -> Option<Draft> {
        match candidate.kind {
            FindingKind::FalseDependency if judged.probability >= FALSE_DEPENDENCY_PROPOSAL_P => {
                let (from, to) = (&candidate.ids[0], &candidate.ids[1]);
                Some(Draft {
                    id: format!("false_dependency:{from}->{to}"),
                    kind: candidate.kind,
                    edits: vec![GraphEdit::RemoveEdge {
                        from: from.clone(),
                        to: to.clone(),
                    }],
                    cost: 0.0,
                    rationale: format!(
                        "Jev: P('{to}' does not need '{from}') = {:.2}; removing the edge",
                        judged.probability
                    ),
                })
            }
            FindingKind::CrashOption
                if judged.confidence >= CRASH_PROPOSAL_CONFIDENCE
                    && (judged.choice == ADD_CAPACITY || judged.choice == REDUCE_SCOPE) =>
            {
                let id = &candidate.ids[0];
                let d = self.deliverable(id)?;
                let length = self.length(id);
                let hours = round_hours(length * CRASH_LENGTH_FACTOR);
                // add_capacity shortens calendar time, not the work: the
                // duration replaces effort as the scheduled length while
                // effort stays the cost basis.
                let edit = if judged.choice == ADD_CAPACITY || d.duration_hours.is_some() {
                    GraphEdit::SetDuration {
                        id: id.clone(),
                        hours: Some(hours),
                    }
                } else {
                    GraphEdit::SetEffort {
                        id: id.clone(),
                        hours,
                    }
                };
                let cost = if judged.choice == ADD_CAPACITY {
                    round_hours(length * ADD_CAPACITY_COST_FACTOR)
                } else {
                    0.0
                };
                Some(Draft {
                    id: format!("crash_option:{}:{id}", judged.choice),
                    kind: candidate.kind,
                    edits: vec![edit],
                    cost,
                    rationale: format!(
                        "Jev picks {} for '{id}' (confidence {:.2}); {} h -> {} h at {} h added \
                         effort",
                        judged.choice,
                        judged.confidence,
                        fmt_hours(length),
                        fmt_hours(hours),
                        fmt_hours(cost)
                    ),
                })
            }
            _ => None,
        }
    }
}

fn instructions(kind: FindingKind, ids: &[String], question: String) -> Value {
    json!({ "kind": kind.as_str(), "ids": ids, "question": question })
}

/// Interleave the lists, one from each in turn, until `cap` are taken.
fn round_robin<const N: usize>(lists: [Vec<Candidate>; N], cap: usize) -> Vec<Candidate> {
    let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for it in &mut iters {
            if out.len() >= cap {
                return out;
            }
            if let Some(c) = it.next() {
                out.push(c);
                progressed = true;
            }
        }
        if !progressed {
            return out;
        }
    }
}

fn question_id(index: usize) -> String {
    format!("q{index:02}")
}

// ---------------------------------------------------------------- answers

struct Judged {
    probability: f32,
    /// Choice answers only.
    choice: String,
    /// Answer confidence for Choice answers; the probability otherwise.
    confidence: f32,
    evidence: String,
}

/// A probability Jev could legitimately return: finite and in `[0, 1]`.
fn probability(value: f64) -> Option<f32> {
    (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(value as f32)
}

fn interpret(candidate: &Candidate, answer: &Answer) -> Option<Judged> {
    let ids = &candidate.ids;
    match (candidate.kind, answer) {
        (
            FindingKind::FalseDependency
            | FindingKind::MissingDependency
            | FindingKind::InterfaceSplit,
            Answer::Noul { noul },
        ) => {
            let p = probability(*noul)?;
            let evidence = match candidate.kind {
                FindingKind::FalseDependency => {
                    format!("P('{}' does not need '{}') = {p:.2}", ids[1], ids[0])
                }
                FindingKind::MissingDependency => {
                    format!("P('{}' and '{}' need an ordering) = {p:.2}", ids[0], ids[1])
                }
                _ => format!(
                    "P('{}' can start against an early interface of '{}') = {p:.2}",
                    ids[1], ids[0]
                ),
            };
            Some(Judged {
                probability: p,
                choice: String::new(),
                confidence: p,
                evidence,
            })
        }
        (
            FindingKind::SplitCandidate,
            Answer::Score {
                score,
                probabilities,
                ..
            },
        ) => {
            let top = SPLIT_LEVELS.len() - 1;
            let score =
                (score.is_finite() && (0.0..=top as f64).contains(score)).then_some(*score)?;
            let mut mass = 0.0f64;
            for (level, p) in probabilities {
                let p = probability(*p)?;
                // Only the asked levels, spelled canonically ("0".."4").
                let index = level.parse::<usize>().ok().filter(|i| *i <= top)?;
                if index.to_string() != *level {
                    return None;
                }
                if index >= SPLIT_LIKELY_LEVEL {
                    mass += f64::from(p);
                }
            }
            let p = probability(mass.min(1.0))?;
            Some(Judged {
                probability: p,
                choice: String::new(),
                confidence: p,
                evidence: format!(
                    "split score {score:.2} of {}; P(level >= {SPLIT_LIKELY_LEVEL}) = {p:.2}",
                    SPLIT_LEVELS.len() - 1
                ),
            })
        }
        (
            FindingKind::CrashOption,
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            },
        ) => {
            let Question::Choice { criteria, .. } = &candidate.question else {
                return None;
            };
            if !criteria.contains_key(choice)
                || probabilities.keys().any(|k| !criteria.contains_key(k))
            {
                return None;
            }
            let p = probability(*probabilities.get(choice)?)?;
            let confidence = probability(*confidence)?;
            Some(Judged {
                // `none` is not a finding.
                probability: if choice == NONE { 0.0 } else { p },
                choice: choice.clone(),
                confidence,
                evidence: format!("Jev picks {choice} for '{}' (P = {p:.2})", ids[0]),
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- verification

struct Draft {
    id: String,
    kind: FindingKind,
    edits: Vec<GraphEdit>,
    cost: f32,
    rationale: String,
}

fn makespan(sim: &SimulationResult) -> f32 {
    sim.resource_schedule
        .as_ref()
        .map_or(sim.critical_path_hours, |r| r.makespan)
}

/// `(code, ids)` of every warning or error finding.
fn lint_issues(report: &LintReport) -> HashSet<(String, Vec<String>)> {
    report
        .findings
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .map(|f| (f.code.clone(), f.ids.clone()))
        .collect()
}

fn verify(
    graph: &PlanGraph,
    sim_req: &SimulateRequest,
    base_issues: &HashSet<(String, Vec<String>)>,
    base_makespan: f32,
    draft: Draft,
) -> Option<Proposal> {
    let edited = apply_edits(graph, &draft.edits).ok()?;
    let after = simulate(&edited, sim_req).ok()?;
    if !lint_issues(&after.lint).is_subset(base_issues) {
        return None;
    }
    let after_makespan = makespan(&after);
    let hours_saved = round_hours(base_makespan - after_makespan);
    if !hours_saved.is_finite() || hours_saved < MIN_HOURS_SAVED {
        return None;
    }
    let score = hours_saved / draft.cost.max(1.0);
    if !score.is_finite() {
        return None;
    }
    Some(Proposal {
        rationale: format!(
            "{}; simulated makespan {} h -> {} h",
            draft.rationale,
            fmt_hours(base_makespan),
            fmt_hours(after_makespan)
        ),
        id: draft.id,
        kind: draft.kind,
        edits: draft.edits,
        hours_saved,
        cost: draft.cost,
        score,
    })
}

// ---------------------------------------------------------------- helpers

/// Round to `1 / HOURS_SCALE` hours, in f64 so the f32 input's binary error
/// cannot push a value across a rounding boundary.
fn round_hours(hours: f32) -> f32 {
    ((f64::from(hours) * HOURS_SCALE).round() / HOURS_SCALE) as f32
}

/// `(a, b)` with the smaller id first.
fn ordered_pair(ds: &[Deliverable], a: usize, b: usize) -> (usize, usize) {
    if ds[a].id <= ds[b].id { (a, b) } else { (b, a) }
}

/// Directory-signal pairs and their weight: [`SHARED_DIR_WEIGHT`] for files
/// in the same directory, [`NESTED_DIR_WEIGHT`] when one file's directory
/// is a proper ancestor of the other's (component-wise; the larger weight
/// wins when both hold).
///
/// Bounds, all deterministic: directories deeper than [`MAX_DIR_DEPTH`]
/// components and top-level files carry no signal; a same-directory group
/// of more than [`MAX_SIGNAL_GROUP`] deliverables is skipped, and so is an
/// ancestor whose subtree (its own entries plus every descendant
/// directory's) has more than [`MAX_SIGNAL_GROUP`] entries. Directories are
/// visited in sorted order (members in deliverable-id order, nearest
/// ancestor first) and generation stops at [`MAX_CANDIDATE_PAIRS`] pairs, so
/// which pairs survive the cap does not depend on declaration order.
pub(crate) fn directory_pairs(ds: &[Deliverable]) -> BTreeMap<(usize, usize), u32> {
    let mut dirs: BTreeMap<&Path, BTreeSet<(&str, usize)>> = BTreeMap::new();
    for (i, d) in ds.iter().enumerate() {
        for f in &d.owned_files {
            if let Some(dir) = f.path().parent()
                && dir.parent().is_some()
                && dir.components().nth(MAX_DIR_DEPTH).is_none()
            {
                dirs.entry(dir).or_default().insert((d.id.as_str(), i));
            }
        }
    }
    let mut subtree: HashMap<&Path, usize> = HashMap::new();
    for (dir, members) in &dirs {
        for ancestor in dir.ancestors().filter(|a| a.parent().is_some()) {
            *subtree.entry(ancestor).or_default() += members.len();
        }
    }

    let mut pairs: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    let mut add = |a: usize, b: usize, weight: u32| {
        if pairs.len() >= MAX_CANDIDATE_PAIRS {
            return false;
        }
        let w = pairs.entry(ordered_pair(ds, a, b)).or_default();
        *w = (*w).max(weight);
        true
    };
    for (dir, members) in &dirs {
        if members.len() <= MAX_SIGNAL_GROUP {
            let list: Vec<usize> = members.iter().map(|(_, i)| *i).collect();
            for (k, &a) in list.iter().enumerate() {
                for &b in &list[k + 1..] {
                    if !add(a, b, SHARED_DIR_WEIGHT) {
                        return pairs;
                    }
                }
            }
        }
        for ancestor in dir.ancestors().skip(1).filter(|a| a.parent().is_some()) {
            if subtree.get(ancestor).is_none_or(|n| *n > MAX_SIGNAL_GROUP) {
                continue;
            }
            let Some(others) = dirs.get(ancestor) else {
                continue;
            };
            for &(_, a) in members {
                for &(_, b) in others.iter().filter(|(_, b)| *b != a) {
                    if !add(a, b, NESTED_DIR_WEIGHT) {
                        return pairs;
                    }
                }
            }
        }
    }
    pairs
}

fn fmt_hours(hours: f32) -> String {
    format!("{hours:.2}")
}

fn truncate(text: &str) -> String {
    text.chars().take(MAX_STATE_TEXT_CHARS).collect()
}

/// Drop a provider-reported cost holding a non-finite figure.
fn sanitize_usage(mut usage: Usage) -> Usage {
    let finite = |v: Option<f64>| v.is_none_or(f64::is_finite);
    if let Some(cost) = &usage.cost
        && !(cost.total.is_finite()
            && finite(cost.input)
            && finite(cost.output)
            && finite(cost.cache_read)
            && finite(cost.cache_write))
    {
        usage.cost = None;
    }
    usage
}

/// sha256 hex of `{ "questions": …, "state": … }`, keys sorted at every
/// level, compact.
fn prompt_hash(state: &Value, questions: &BTreeMap<String, Question>) -> String {
    let request = json!({ "state": state, "questions": questions });
    let mut canonical = String::new();
    write_canonical(&request, &mut canonical);
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::OwnedFile;
    use std::path::PathBuf;

    fn owning(id: String, path: String) -> Deliverable {
        Deliverable {
            id,
            owned_files: vec![OwnedFile::Path(PathBuf::from(path))],
            prerequisites: Vec::new(),
            estimated_effort_hours: Some(1.0),
            duration_hours: None,
            estimate: None,
            metadata: Value::Null,
            milestone: false,
        }
    }

    #[test]
    fn deep_directory_chain_keeps_pair_count_bounded() {
        // Deliverable i owns a file i + 1 directories deep, one chain.
        let mut dir = String::new();
        let ds: Vec<Deliverable> = (0..5000)
            .map(|i| {
                dir.push_str("d/");
                owning(format!("x{i:04}"), format!("{dir}f.rs"))
            })
            .collect();
        assert!(directory_pairs(&ds).len() <= MAX_CANDIDATE_PAIRS);
    }

    #[test]
    fn wide_directory_tree_stops_at_the_pair_cap() {
        // 1000 sibling directories of 20 deliverables each: 190k
        // same-directory pairs uncapped.
        let ds: Vec<Deliverable> = (0..20_000)
            .map(|i| owning(format!("x{i:05}"), format!("src/m{:03}/f{i}.rs", i / 20)))
            .collect();
        assert_eq!(directory_pairs(&ds).len(), MAX_CANDIDATE_PAIRS);
    }

    /// Pairs named by id, so two declaration orders can be compared.
    fn id_pairs(ds: &[Deliverable]) -> BTreeMap<(String, String), u32> {
        directory_pairs(ds)
            .into_iter()
            .map(|((a, b), w)| ((ds[a].id.clone(), ds[b].id.clone()), w))
            .collect()
    }

    #[test]
    fn pair_selection_is_independent_of_declaration_order() {
        // The cap is hit mid-directory, so the member order decides which
        // pairs of that directory survive.
        let ds: Vec<Deliverable> = (0..20_000)
            .map(|i| owning(format!("x{i:05}"), format!("src/m{:03}/f{i}.rs", i / 20)))
            .collect();
        let mut reversed = ds.clone();
        reversed.reverse();
        assert_eq!(id_pairs(&ds), id_pairs(&reversed));
    }
}
