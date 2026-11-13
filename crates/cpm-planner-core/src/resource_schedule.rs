//! Resource-constrained scheduling: levels a plan against per-resource
//! capacities with event-driven list scheduling, then reports the driving
//! chain (what each link waited on) and project / feeding buffers.
//!
//! With a [`ScheduleCalendar`] ([`resource_schedule_on_calendar`]) hours are
//! wall-clock hours from the plan start and each deliverable advances only
//! inside its resource's calendar windows (`docs/calendars.md`, section 3).
//! Without one the output is exactly the calendar-free schedule.

use crate::calendar::{CalendarMap, CalendarSpec, ScheduleCalendar, WorkWindows, instant_after};
use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, FINISH_ID, PlanGraph, PlannerError};
use crate::task::CriticalPathResult;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Add, Sub};

const EPS: f32 = 1e-4;
const UNASSIGNED: &str = "unassigned";
const WALL_CLOCK: &str = "wall_clock";

/// Default `resource_key`: deliverables name their resource in
/// `metadata.owner`.
#[doc(hidden)]
pub fn default_resource_key() -> String {
    "owner".to_string()
}

/// Default `project_buffer_pct`.
#[doc(hidden)]
pub fn default_buffer_pct() -> f32 {
    25.0
}

/// Check `project_buffer_pct` is a finite percentage in `0..=100`.
#[doc(hidden)]
pub fn check_buffer_pct(pct: f32) -> Result<(), String> {
    if pct.is_finite() && (0.0..=100.0).contains(&pct) {
        Ok(())
    } else {
        Err(format!(
            "project_buffer_pct must be between 0 and 100; got {pct}"
        ))
    }
}

/// Input to [`resource_schedule`].
///
/// A deliverable uses its legacy `metadata.<resource_key>` pool unless it
/// declares `metadata.resources` as a map of pool names to positive unit
/// claims. Every listed pool is reserved simultaneously. A deliverable with
/// `metadata.wall_clock_only: true` must declare `duration_hours`; it advances
/// elapsed time and dependencies but claims no capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    /// Units available per resource name.
    pub capacities: BTreeMap<String, u32>,
    /// Metadata key naming a deliverable's resource.
    #[serde(default = "default_resource_key")]
    pub resource_key: String,
    /// Buffer size as a percentage of the chain's scheduled length.
    #[serde(default = "default_buffer_pct")]
    pub project_buffer_pct: f32,
}

/// One deliverable's leveled position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRow {
    pub id: String,
    /// Legacy display name for the primary pool. Multi-pool claims are
    /// represented precisely in `resources`.
    pub resource: String,
    /// Every capacity pool claimed concurrently by this deliverable.
    /// Empty means a `wall_clock_only` observation window.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, u32>,
    pub start: f32,
    pub finish: f32,
    /// `start` as an RFC 3339 UTC instant; calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<DateTime<Utc>>,
    /// `finish` as an RFC 3339 UTC instant; calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_at: Option<DateTime<Utc>>,
    /// Calendar keys whose windows this deliverable works in (`[]` for
    /// `wall_clock_only`); calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendars: Option<Vec<String>>,
    /// Wall-clock hours from `unconstrained` (ready and resource free) to
    /// `start`; calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_wait_hours: Option<f32>,
}

/// Busy time of one resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceLoad {
    pub resource: String,
    pub capacity: u32,
    pub busy_hours: f32,
    /// `capacity` times the working hours of the pool's calendar in
    /// `[0, makespan)`; calendar schedules only (without a calendar it is
    /// `capacity * makespan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_hours: Option<f32>,
    /// `busy_hours / available_hours` (0 when nothing is available).
    pub utilisation: f32,
}

/// What a driving-chain link waited on before starting. For `Resource`, the
/// concrete saturated pool is reported in [`ChainLink::waited_on_resource`];
/// for `Calendar`, the calendar keys in [`ChainLink::waited_on_calendar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Start,
    Dependency,
    Resource,
    Calendar,
}

/// One link of the driving chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainLink {
    pub id: String,
    pub waited_on: WaitKind,
    /// The exact saturated pool when `waited_on` is `resource` (or a
    /// calendar link whose `unconstrained` time came from that pool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited_on_resource: Option<String>,
    /// The link's calendar keys joined with `+` when `waited_on` is
    /// `calendar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited_on_calendar: Option<String>,
    /// Wall-clock hours the link waited for its window to open, when
    /// `waited_on` is `calendar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_wait_hours: Option<f32>,
}

/// Buffer protecting the point where a side chain joins the driving chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedingBuffer {
    pub joins: String,
    pub from_chain: Vec<String>,
    pub buffer_hours: f32,
}

/// Result of [`resource_schedule`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSchedule {
    /// The plan start hour 0 refers to; calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    pub makespan: f32,
    /// Unconstrained CPM makespan, for comparison (on a calendar, the
    /// calendar CPM forward pass on the same windows).
    pub cpm_makespan: f32,
    /// Graph order, no synthetic rows.
    pub rows: Vec<ScheduledRow>,
    /// Sorted by resource.
    pub load: Vec<ResourceLoad>,
    /// Execution order; the first link waited on `Start` (or `Calendar`
    /// when the plan start falls outside its window).
    pub driving_chain: Vec<ChainLink>,
    /// Percentage of the summed scheduled lengths on `driving_chain`.
    pub project_buffer_hours: f32,
    pub feeding_buffers: Vec<FeedingBuffer>,
    /// Summed calendar wait of the driving chain's links, in wall-clock
    /// hours; calendar schedules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_wait_hours: Option<f32>,
}

struct Node {
    len: f32,
    resource: String,
    resources: BTreeMap<String, u32>,
    /// (predecessor index, lag); a repeated id keeps its largest lag.
    preds: Vec<(usize, f32)>,
}

/// Schedule hours: `f32` on the calendar-free path (its arithmetic is
/// unchanged), `f64` on the resource clock.
trait Hour: Copy + PartialOrd + Add<Output = Self> + Sub<Output = Self> {
    const ZERO: Self;
    const EPS: Self;
    const NAN: Self;
    fn from_f32(v: f32) -> Self;
    fn to_f32(self) -> f32;
    fn abs(self) -> Self;
    fn max(self, other: Self) -> Self;
    fn total_cmp(&self, other: &Self) -> Ordering;
    fn is_nan(self) -> bool;
}

impl Hour for f32 {
    const ZERO: Self = 0.0;
    const EPS: Self = EPS;
    const NAN: Self = f32::NAN;
    fn from_f32(v: f32) -> Self {
        v
    }
    fn to_f32(self) -> f32 {
        self
    }
    fn abs(self) -> Self {
        f32::abs(self)
    }
    fn max(self, other: Self) -> Self {
        f32::max(self, other)
    }
    fn total_cmp(&self, other: &Self) -> Ordering {
        f32::total_cmp(self, other)
    }
    fn is_nan(self) -> bool {
        f32::is_nan(self)
    }
}

impl Hour for f64 {
    const ZERO: Self = 0.0;
    const EPS: Self = EPS as f64;
    const NAN: Self = f64::NAN;
    fn from_f32(v: f32) -> Self {
        f64::from(v)
    }
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn abs(self) -> Self {
        f64::abs(self)
    }
    fn max(self, other: Self) -> Self {
        f64::max(self, other)
    }
    fn total_cmp(&self, other: &Self) -> Ordering {
        f64::total_cmp(self, other)
    }
    fn is_nan(self) -> bool {
        f64::is_nan(self)
    }
}

/// Where each deliverable landed, and its `unconstrained` time: the
/// latest of ready and resource-free, before any calendar wait.
struct Placed<H> {
    start: Vec<H>,
    finish: Vec<H>,
    unconstrained: Vec<H>,
}

/// `place(i, unconstrained)`: start and finish of deliverable `i`.
type Place<'a, H> = &'a dyn Fn(usize, H) -> (H, H);

/// One driving-chain step: (index, wait kind, saturated pool, calendar wait).
type ChainStep<H> = (usize, WaitKind, Option<String>, Option<H>);

/// The resource clock of one schedule: each deliverable's calendar keys and
/// windows (graph order), and the map and start they came from.
#[doc(hidden)]
pub struct Clock {
    map: CalendarMap,
    start: DateTime<Utc>,
    keys: Vec<Vec<String>>,
    windows: Vec<WorkWindows>,
}

impl Clock {
    /// Resolve every deliverable's calendar keys and windows (section 2.5).
    /// `pools[i]` lists the pools deliverable `i` claims; empty (a
    /// `wall_clock_only` deliverable) is always open.
    #[doc(hidden)]
    pub fn new(
        graph: &PlanGraph,
        pools: &[Vec<String>],
        when: &ScheduleCalendar,
    ) -> Result<Self, PlannerError> {
        if let CalendarSpec::Single(c) = &when.calendar {
            c.validate()?;
        }
        let map = when.calendar.to_map();
        let mut keys = Vec::with_capacity(pools.len());
        let mut windows = Vec::with_capacity(pools.len());
        for (d, pools) in graph.deliverables.iter().zip(pools) {
            let k = map.keys_for(pools.iter().map(String::as_str));
            let w = map
                .windows(&k, when.start)
                .ok_or_else(|| PlannerError::InvalidValue {
                    reason: format!(
                        "deliverable {:?}: calendars [{}] never overlap",
                        d.id,
                        k.join(", ")
                    ),
                })?;
            keys.push(k);
            windows.push(w);
        }
        Ok(Self {
            map,
            start: when.start,
            keys,
            windows,
        })
    }

    /// The windows of deliverable `i` (graph order).
    #[doc(hidden)]
    pub fn windows(&self, i: usize) -> &WorkWindows {
        &self.windows[i]
    }

    /// Placement rule (section 3.3): work of `len > 0` starts at the next
    /// open at or after `u` and advances through the windows; anything else
    /// is an instant at `u`.
    #[doc(hidden)]
    pub fn place(&self, i: usize, u: f64, len: f64) -> (f64, f64) {
        if len <= 0.0 {
            return (u, u);
        }
        let s = self.windows[i].open_at(u);
        (s, self.windows[i].advance(s, len))
    }

    fn instant(&self, hours: f64) -> DateTime<Utc> {
        instant_after(self.start, hours)
    }
}

/// Level `graph` against `req.capacities`.
///
/// The graph is validated exactly as `plan.submit` validates it, so a
/// reserved or duplicate id, a cycle, an unknown prerequisite or an
/// out-of-range hour value is [`PlannerError::InvalidGraph`].
pub fn resource_schedule(
    graph: &PlanGraph,
    req: &ScheduleRequest,
) -> Result<ResourceSchedule, PlannerError> {
    crate::validate::validate_graph(graph)?;
    let cpm = crate::schedule::compute_cpm(graph)?;
    resource_schedule_with_cpm(graph, req, &cpm)
}

/// [`resource_schedule`] on per-resource calendars (`docs/calendars.md`,
/// section 3), or exactly [`resource_schedule`] when `calendar` is `None`.
///
/// On a calendar, `start`, `finish` and `makespan` are wall-clock hours
/// from `calendar.start`; rows add `start_at`, `finish_at`, `calendars`
/// and `calendar_wait_hours`, loads add `available_hours`, and driving-chain
/// links that waited for a window report `waited_on: "calendar"`. A
/// multi-pool deliverable whose calendars never overlap is
/// [`PlannerError::InvalidValue`].
pub fn resource_schedule_on_calendar(
    graph: &PlanGraph,
    req: &ScheduleRequest,
    calendar: Option<&ScheduleCalendar>,
) -> Result<ResourceSchedule, PlannerError> {
    match calendar {
        None => resource_schedule(graph, req),
        Some(calendar) => {
            crate::validate::validate_graph(graph)?;
            calendar_schedule(graph, req, calendar)
        }
    }
}

/// The pools each deliverable claims, exactly as levelling claims them
/// (keys of `metadata.resources`, else `metadata.<key>`, else
/// `unassigned`; empty for `wall_clock_only`), for calendar lookup.
#[doc(hidden)]
pub fn pools_of(graph: &PlanGraph, key: &str) -> Result<Vec<Vec<String>>, PlannerError> {
    Ok(build_nodes(graph, key)?
        .into_iter()
        .map(|n| n.resources.into_keys().collect())
        .collect())
}

/// [`resource_schedule_with_cpm`] on the resource clock, over a validated
/// graph.
#[doc(hidden)]
pub fn calendar_schedule(
    graph: &PlanGraph,
    req: &ScheduleRequest,
    when: &ScheduleCalendar,
) -> Result<ResourceSchedule, PlannerError> {
    check_buffer_pct(req.project_buffer_pct)
        .map_err(|reason| PlannerError::InvalidGraph { reason })?;
    let nodes = build_nodes(graph, &req.resource_key)?;
    let pools: Vec<Vec<String>> = nodes
        .iter()
        .map(|n| n.resources.keys().cloned().collect())
        .collect();
    let clock = Clock::new(graph, &pools, when)?;
    check_capacities(&nodes, &req.capacities)?;
    let order = topological_order(&nodes)?;
    let tail = tails(&nodes, &order);
    let ids: Vec<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    let place = |i: usize, u: f64| clock.place(i, u, f64::from(nodes[i].len));

    let cpm = forward(&nodes, &order, &place);
    let cpm_makespan = cpm.finish.iter().copied().fold(0.0_f64, f64::max);
    let placed = level(&nodes, &ids, &tail, &req.capacities, &place)?;
    let makespan = placed.finish.iter().copied().fold(0.0_f64, f64::max);

    let rows = (0..nodes.len())
        .map(|i| ScheduledRow {
            id: ids[i].to_string(),
            resource: nodes[i].resource.clone(),
            resources: nodes[i].resources.clone(),
            start: placed.start[i] as f32,
            finish: placed.finish[i] as f32,
            start_at: Some(clock.instant(placed.start[i])),
            finish_at: Some(clock.instant(placed.finish[i])),
            calendars: Some(clock.keys[i].clone()),
            calendar_wait_hours: Some((placed.start[i] - placed.unconstrained[i]) as f32),
        })
        .collect();

    let chain = driving_chain(&nodes, &ids, &placed, &req.capacities);
    let chain_set: BTreeSet<usize> = chain.iter().map(|step| step.0).collect();
    let pct = req.project_buffer_pct / 100.0;
    let project_buffer_hours = pct * chain.iter().map(|step| nodes[step.0].len).sum::<f32>();
    let feeding_buffers = feeding_buffers(&nodes, &ids, &order, &chain_set, pct);
    let wait_total: f64 = chain.iter().filter_map(|step| step.3).sum();
    let available = |resource: &str, capacity: u32| -> f32 {
        let hours = WorkWindows::of(clock.map.calendar_for(resource), clock.start)
            .working_hours(0.0, makespan);
        (f64::from(capacity) * hours) as f32
    };

    Ok(ResourceSchedule {
        start: Some(clock.start),
        makespan: makespan as f32,
        cpm_makespan: cpm_makespan as f32,
        rows,
        load: loads(&nodes, &req.capacities, Some(&available), makespan as f32),
        driving_chain: chain_links(&ids, &clock.keys, chain),
        project_buffer_hours,
        feeding_buffers,
        calendar_wait_hours: Some(wait_total as f32),
    })
}

/// [`resource_schedule`] over an already validated graph and its CPM
/// result, so callers that already ran CPM (`simulate`) do not repeat it.
#[doc(hidden)]
pub fn resource_schedule_with_cpm(
    graph: &PlanGraph,
    req: &ScheduleRequest,
    cpm: &CriticalPathResult,
) -> Result<ResourceSchedule, PlannerError> {
    check_buffer_pct(req.project_buffer_pct)
        .map_err(|reason| PlannerError::InvalidGraph { reason })?;
    let cpm_makespan = cpm.get_task(FINISH_ID).map_or(0.0, |t| t.earliest_finish);

    let nodes = build_nodes(graph, &req.resource_key)?;
    check_capacities(&nodes, &req.capacities)?;
    let order = topological_order(&nodes)?;
    let tail = tails(&nodes, &order);
    let ids: Vec<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    let place = |i: usize, u: f32| (u, u + nodes[i].len);
    let placed = level(&nodes, &ids, &tail, &req.capacities, &place)?;
    let (start, finish) = (&placed.start, &placed.finish);

    let makespan = finish.iter().copied().fold(0.0_f32, f32::max);
    let rows = (0..nodes.len())
        .map(|i| ScheduledRow {
            id: ids[i].to_string(),
            resource: nodes[i].resource.clone(),
            resources: nodes[i].resources.clone(),
            start: start[i],
            finish: finish[i],
            start_at: None,
            finish_at: None,
            calendars: None,
            calendar_wait_hours: None,
        })
        .collect();

    let chain = driving_chain(&nodes, &ids, &placed, &req.capacities);
    let chain_set: BTreeSet<usize> = chain.iter().map(|step| step.0).collect();
    let pct = req.project_buffer_pct / 100.0;
    let project_buffer_hours = pct * chain.iter().map(|step| nodes[step.0].len).sum::<f32>();
    let feeding_buffers = feeding_buffers(&nodes, &ids, &order, &chain_set, pct);

    Ok(ResourceSchedule {
        start: None,
        makespan,
        cpm_makespan,
        rows,
        load: loads(&nodes, &req.capacities, None, makespan),
        driving_chain: chain_links(&ids, &[], chain),
        project_buffer_hours,
        feeding_buffers,
        calendar_wait_hours: None,
    })
}

/// The driving chain as wire links; calendar links name their keys.
fn chain_links<H: Hour>(
    ids: &[&str],
    keys: &[Vec<String>],
    chain: Vec<ChainStep<H>>,
) -> Vec<ChainLink> {
    chain
        .into_iter()
        .map(|(i, waited_on, waited_on_resource, wait)| ChainLink {
            id: ids[i].to_string(),
            waited_on,
            waited_on_resource,
            waited_on_calendar: wait.and_then(|_| keys.get(i)).map(|k| k.join("+")),
            calendar_wait_hours: wait.map(Hour::to_f32),
        })
        .collect()
}

/// Calendar CPM forward pass (section 3.6): placement without levelling.
fn forward<H: Hour>(nodes: &[Node], order: &[usize], place: Place<'_, H>) -> Placed<H> {
    let n = nodes.len();
    let mut out = Placed {
        start: vec![H::ZERO; n],
        finish: vec![H::ZERO; n],
        unconstrained: vec![H::ZERO; n],
    };
    for &i in order {
        let u = nodes[i]
            .preds
            .iter()
            .map(|&(p, lag)| out.finish[p] + H::from_f32(lag))
            .fold(H::ZERO, H::max);
        let (s, f) = place(i, u);
        out.unconstrained[i] = u;
        out.start[i] = s;
        out.finish[i] = f;
    }
    out
}

/// Event-driven list scheduling, shared by both clocks. Each step places
/// the candidate (every predecessor placed) with the earliest `place`d
/// start; ties go to milestones, then the longest tail, then the id.
fn level<H: Hour>(
    nodes: &[Node],
    ids: &[&str],
    tail: &[f32],
    capacities: &BTreeMap<String, u32>,
    place: Place<'_, H>,
) -> Result<Placed<H>, PlannerError> {
    let n = nodes.len();
    // Allocate units only for resources that carry work, and never more than
    // the number of work-bearing deliverables on that resource: extra units
    // could never be occupied, and sizing by raw capacity can demand huge
    // allocations for large (even u32::MAX) capacities.
    let mut work_per_resource: BTreeMap<&str, usize> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.len > 0.0) {
        for (resource, claim) in &n.resources {
            *work_per_resource.entry(resource).or_insert(0) += *claim as usize;
        }
    }
    let mut units: BTreeMap<String, Vec<H>> = work_per_resource
        .into_iter()
        .map(|(resource, work)| {
            let capacity = capacities
                .get(resource)
                .copied()
                .expect("INVARIANT: capacities validated before allocation")
                as usize;
            (resource.to_string(), vec![H::ZERO; capacity.min(work)])
        })
        .collect();
    let mut start = vec![H::NAN; n];
    let mut finish = vec![H::NAN; n];
    let mut unconstrained = vec![H::NAN; n];
    let mut done = 0;
    while done < n {
        // Candidate: every predecessor placed; its earliest feasible start.
        let mut best: Option<(usize, H, H, H)> = None; // (idx, start, finish, unconstrained)
        for i in 0..n {
            if !start[i].is_nan() || nodes[i].preds.iter().any(|&(p, _)| start[p].is_nan()) {
                continue;
            }
            let ready = nodes[i]
                .preds
                .iter()
                .map(|&(p, lag)| finish[p] + H::from_f32(lag))
                .fold(H::ZERO, H::max);
            let mut u = ready;
            if nodes[i].len > 0.0 {
                for (resource, claim) in &nodes[i].resources {
                    let mut availability = units[resource].clone();
                    availability.sort_by(H::total_cmp);
                    let free = availability[*claim as usize - 1];
                    u = u.max(free);
                }
            }
            let (s, f) = place(i, u);
            let better = match best {
                None => true,
                Some((b, bs, _, _)) => {
                    if (s - bs).abs() > H::EPS {
                        s < bs
                    } else {
                        let (zi, zb) = (nodes[i].len <= 0.0, nodes[b].len <= 0.0);
                        if zi != zb {
                            zi
                        } else if (tail[i] - tail[b]).abs() > EPS {
                            tail[i] > tail[b]
                        } else {
                            ids[i] < ids[b]
                        }
                    }
                }
            };
            if better {
                best = Some((i, s, f, u));
            }
        }
        let Some((i, s, f, u)) = best else {
            return Err(PlannerError::InvalidGraph {
                reason: "resource schedule could not place every deliverable".to_string(),
            });
        };
        start[i] = s;
        finish[i] = f;
        unconstrained[i] = u;
        if nodes[i].len > 0.0 {
            for (resource, claim) in &nodes[i].resources {
                let pool = units
                    .get_mut(resource)
                    .expect("INVARIANT: capacity validated before allocation");
                let mut free: Vec<usize> = (0..pool.len()).collect();
                free.sort_by(|&a, &b| pool[a].total_cmp(&pool[b]));
                for unit in free.into_iter().take(*claim as usize) {
                    pool[unit] = finish[i];
                }
            }
        }
        done += 1;
    }
    Ok(Placed {
        start,
        finish,
        unconstrained,
    })
}

fn build_nodes(graph: &PlanGraph, key: &str) -> Result<Vec<Node>, PlannerError> {
    let estimator = EffortEstimator::new();
    let index: HashMap<&str, usize> = graph
        .deliverables
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id.as_str(), i))
        .collect();
    graph
        .deliverables
        .iter()
        .map(|d| {
            let mut preds: BTreeMap<usize, f32> = BTreeMap::new();
            for p in &d.prerequisites {
                let &pi = index
                    .get(p.id())
                    .ok_or_else(|| PlannerError::InvalidGraph {
                        reason: format!("{} lists unknown prerequisite {}", d.id, p.id()),
                    })?;
                let lag = preds.entry(pi).or_insert(0.0);
                *lag = lag.max(p.lag_hours().max(0.0));
            }
            let wall_clock_only = d
                .metadata
                .get("wall_clock_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if wall_clock_only && d.duration_hours.is_none() {
                return Err(PlannerError::InvalidGraph {
                    reason: format!("{} marks wall_clock_only but has no duration_hours", d.id),
                });
            }
            let resources = if wall_clock_only {
                BTreeMap::new()
            } else {
                resources_of(d, key)?
            };
            let resource = if resources.is_empty() {
                WALL_CLOCK.to_string()
            } else {
                resources.keys().cloned().collect::<Vec<_>>().join("+")
            };
            Ok(Node {
                len: crate::schedule::scheduled_length(d, &estimator),
                resource,
                resources,
                preds: preds.into_iter().collect(),
            })
        })
        .collect()
}

fn resources_of(d: &Deliverable, key: &str) -> Result<BTreeMap<String, u32>, PlannerError> {
    let Some(resources) = d.metadata.get("resources") else {
        let resource = d
            .metadata
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(UNASSIGNED)
            .to_string();
        return Ok(BTreeMap::from([(resource, 1)]));
    };
    let object = resources
        .as_object()
        .ok_or_else(|| PlannerError::InvalidGraph {
            reason: format!("{} metadata.resources must be an object", d.id),
        })?;
    if object.is_empty() {
        return Err(PlannerError::InvalidGraph {
            reason: format!("{} metadata.resources must not be empty", d.id),
        });
    }
    object
        .iter()
        .map(|(resource, value)| {
            let claim = value
                .as_u64()
                .filter(|&n| n > 0 && n <= u32::MAX as u64)
                .ok_or_else(|| PlannerError::InvalidGraph {
                    reason: format!(
                        "{} metadata.resources.{resource} must be a positive integer",
                        d.id
                    ),
                })? as u32;
            if resource.is_empty() {
                return Err(PlannerError::InvalidGraph {
                    reason: format!("{} metadata.resources must not name an empty pool", d.id),
                });
            }
            Ok((resource.clone(), claim))
        })
        .collect()
}

/// Only work that occupies time needs capacity; milestones are exempt.
fn check_capacities(nodes: &[Node], caps: &BTreeMap<String, u32>) -> Result<(), PlannerError> {
    let missing: BTreeSet<&String> = nodes
        .iter()
        .filter(|n| n.len > 0.0)
        .flat_map(|n| n.resources.iter())
        .filter(|(r, claim)| caps.get(*r).copied().unwrap_or(0) < **claim)
        .map(|(r, _)| r)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PlannerError::InvalidCapacities {
            missing: missing.into_iter().cloned().collect(),
        })
    }
}

/// Kahn order over `preds`; errors on a cycle (unreachable once the graph
/// is validated, but never loop on it).
fn topological_order(nodes: &[Node]) -> Result<Vec<usize>, PlannerError> {
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &(p, _) in &n.preds {
            succs[p].push(i);
        }
    }
    let mut indeg: Vec<usize> = nodes.iter().map(|n| n.preds.len()).collect();
    let mut order: Vec<usize> = (0..nodes.len()).filter(|&i| indeg[i] == 0).collect();
    let mut head = 0;
    while head < order.len() {
        let u = order[head];
        head += 1;
        for &v in &succs[u] {
            indeg[v] -= 1;
            if indeg[v] == 0 {
                order.push(v);
            }
        }
    }
    if order.len() == nodes.len() {
        Ok(order)
    } else {
        Err(PlannerError::InvalidGraph {
            reason: "resource schedule found a prerequisite cycle".to_string(),
        })
    }
}

/// Longest remaining tail per node: its length plus the heaviest successor
/// (edge lag + that successor's tail). Filled in reverse topological order,
/// so no recursion.
fn tails(nodes: &[Node], order: &[usize]) -> Vec<f32> {
    let mut succs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &(p, lag) in &n.preds {
            succs[p].push((i, lag));
        }
    }
    let mut tail = vec![0.0_f32; nodes.len()];
    for &i in order.iter().rev() {
        let best = succs[i]
            .iter()
            .map(|&(s, lag)| lag + tail[s])
            .fold(0.0_f32, f32::max);
        tail[i] = nodes[i].len + best;
    }
    tail
}

/// Walk the driving chain back from the last finish (section 3.5). Each
/// link that started later than its `unconstrained` time waited on its
/// calendar; the walk always continues from `unconstrained`. On the
/// calendar-free clock `unconstrained == start`, so no link is a calendar
/// link.
fn driving_chain<H: Hour>(
    nodes: &[Node],
    ids: &[&str],
    placed: &Placed<H>,
    capacities: &BTreeMap<String, u32>,
) -> Vec<ChainStep<H>> {
    let (start, finish) = (&placed.start, &placed.finish);
    let Some(mut cur) = (0..nodes.len()).min_by(|&a, &b| {
        finish[b]
            .total_cmp(&finish[a])
            .then_with(|| ids[a].cmp(ids[b]))
    }) else {
        return Vec::new();
    };
    let mut rev = Vec::new();
    loop {
        let s = placed.unconstrained[cur];
        let wait = (start[cur] - s > H::EPS).then(|| start[cur] - s);
        let kind = |k: WaitKind| {
            if wait.is_some() {
                WaitKind::Calendar
            } else {
                k
            }
        };
        if s <= H::EPS {
            rev.push((cur, kind(WaitKind::Start), None, wait));
            break;
        }
        let dep = nodes[cur]
            .preds
            .iter()
            .filter(|&&(p, lag)| (finish[p] + H::from_f32(lag) - s).abs() <= H::EPS)
            .map(|&(p, _)| p)
            .min_by(|&a, &b| ids[a].cmp(ids[b]));
        if let Some(p) = dep {
            rev.push((cur, kind(WaitKind::Dependency), None, wait));
            cur = p;
            continue;
        }
        let res = nodes[cur]
            .resources
            .keys()
            .filter(|resource| {
                let occupied: u32 = nodes
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| {
                        *j != cur && start[*j] < s - H::EPS && finish[*j] >= s - H::EPS
                    })
                    .map(|(_, node)| node.resources.get(*resource).copied().unwrap_or(0))
                    .sum();
                occupied + nodes[cur].resources[*resource]
                    > capacities.get(*resource).copied().unwrap_or(0)
            })
            .flat_map(|resource| {
                (0..nodes.len()).filter_map(move |j| {
                    (j != cur
                        && nodes[j].len > 0.0
                        && nodes[j].resources.contains_key(resource)
                        && (finish[j] - s).abs() <= H::EPS)
                        .then_some((j, resource.clone()))
                })
            })
            .min_by(|(a, ar), (b, br)| ids[*a].cmp(ids[*b]).then_with(|| ar.cmp(br)));
        if let Some((j, resource)) = res {
            rev.push((cur, kind(WaitKind::Resource), Some(resource), wait));
            cur = j;
            continue;
        }
        rev.push((cur, kind(WaitKind::Start), None, wait));
        break;
    }
    rev.reverse();
    rev
}

fn feeding_buffers(
    nodes: &[Node],
    ids: &[&str],
    order: &[usize],
    chain: &BTreeSet<usize>,
    pct: f32,
) -> Vec<FeedingBuffer> {
    // Heaviest off-chain dependency chain ending at each node, filled in
    // topological order: (summed length, best off-chain predecessor, first
    // node of that chain). Ties go to the chain whose first id is smaller.
    let mut sum = vec![0.0_f32; nodes.len()];
    let mut via: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut head: Vec<usize> = (0..nodes.len()).collect();
    for &i in order {
        let mut top: Option<usize> = None;
        for &(p, _) in nodes[i].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            let take = match top {
                None => true,
                Some(t) => {
                    sum[p] > sum[t] + EPS
                        || ((sum[p] - sum[t]).abs() <= EPS && ids[head[p]] < ids[head[t]])
                }
            };
            if take {
                top = Some(p);
            }
        }
        sum[i] = top.map_or(0.0, |t| sum[t]) + nodes[i].len;
        via[i] = top;
        if let Some(t) = top {
            head[i] = head[t];
        }
    }
    let path_to = |end: usize| -> Vec<usize> {
        let mut path = vec![end];
        let mut cur = end;
        while let Some(p) = via[cur] {
            path.push(p);
            cur = p;
        }
        path.reverse();
        path
    };

    let mut out = Vec::new();
    for &c in chain {
        for &(p, _) in nodes[c].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            out.push(FeedingBuffer {
                joins: ids[c].to_string(),
                from_chain: path_to(p).iter().map(|&i| ids[i].to_string()).collect(),
                buffer_hours: pct * sum[p],
            });
        }
    }
    out.sort_by(|a, b| {
        a.joins
            .cmp(&b.joins)
            .then_with(|| a.from_chain.first().cmp(&b.from_chain.first()))
    });
    out
}

/// Per-pool working hours available to a schedule: `capacity` times the
/// pool calendar's working hours in `[0, makespan)`.
type Available<'a> = &'a dyn Fn(&str, u32) -> f32;

/// Busy hours per pool and utilisation against `available` (calendar
/// schedules) or `capacity * makespan` (calendar-free).
fn loads(
    nodes: &[Node],
    caps: &BTreeMap<String, u32>,
    available: Option<Available<'_>>,
    makespan: f32,
) -> Vec<ResourceLoad> {
    let mut busy: BTreeMap<&str, f32> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.len > 0.0) {
        for (resource, claim) in &n.resources {
            *busy.entry(resource).or_insert(0.0) += n.len * *claim as f32;
        }
    }
    busy.into_iter()
        .map(|(resource, busy_hours)| {
            let capacity = caps
                .get(resource)
                .copied()
                .expect("INVARIANT: capacities validated before allocation");
            let hours = match available {
                Some(f) => f(resource, capacity),
                None => capacity as f32 * makespan,
            };
            ResourceLoad {
                resource: resource.to_string(),
                capacity,
                busy_hours,
                available_hours: available.map(|_| hours),
                utilisation: if hours > 0.0 { busy_hours / hours } else { 0.0 },
            }
        })
        .collect()
}
