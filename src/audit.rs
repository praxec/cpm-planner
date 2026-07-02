//! Minimal audit surface for the planner's lock-lifecycle events.
//!
//! The planner emits an [`AuditEvent`] for every lock state transition
//! (acquired / released / expired / force-released) and drains them to an
//! [`AuditSink`]. The default [`NullAuditSink`] drops them; an embedder can
//! supply its own sink (file, syslog, a host's audit log) by implementing the
//! trait.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// A single audit record. Builder-style: `AuditEvent::new(type).with_*(…)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    pub correlation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub event_type: String,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl AuditEvent {
    pub fn new(event_type: impl Into<String>) -> Self {
        Self {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            timestamp: Utc::now(),
            workflow_id: None,
            correlation_id: format!("cor_{}", Uuid::new_v4().simple()),
            actor: None,
            event_type: event_type.into(),
            payload: json!({}),
            trace_id: None,
            run_id: None,
        }
    }

    pub fn with_workflow(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    pub fn with_correlation(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = correlation_id.into();
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
}

/// A destination for [`AuditEvent`]s.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()>;

    /// Return all recorded events, or `None` if the sink doesn't retain them.
    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Drops every event. The default when audit isn't configured.
pub struct NullAuditSink;

#[async_trait::async_trait]
impl AuditSink for NullAuditSink {
    async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Writes one JSON line per event to stderr (stdout is the MCP transport
/// channel, so audit narration goes to the diagnostic stream).
pub struct StderrAuditSink;

#[async_trait::async_trait]
impl AuditSink for StderrAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        eprintln!("{}", serde_json::to_string(&event)?);
        Ok(())
    }
}

/// Stores events in memory. Cheap; useful for tests and short-lived processes.
#[derive(Default, Clone)]
pub struct MemoryAuditSink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl MemoryAuditSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .clone()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .clear();
    }
}

#[async_trait::async_trait]
impl AuditSink for MemoryAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .push(event);
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        Some(self.snapshot())
    }
}
