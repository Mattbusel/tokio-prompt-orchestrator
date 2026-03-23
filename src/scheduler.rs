//! Cron-style scheduler for periodic LLM prompt submissions.
//!
//! This module lets you register named prompt templates paired with a
//! cron-like schedule expression.  A background Tokio task wakes up at the
//! right wall-clock times and submits the prompt through the pipeline's
//! standard input channel.
//!
//! ## Supported cron expressions
//!
//! The parser supports a **simplified** subset of cron:
//!
//! | Field | Positions | Accepted values |
//! |-------|-----------|-----------------|
//! | minute | 0 | `*`, `*/N`, `0`–`59` |
//! | hour | 1 | `*`, `*/N`, `0`–`23` |
//!
//! Only `minute` and `hour` fields are parsed (a two-field mini-cron).
//! Second-level precision is not supported.
//!
//! ### Expression examples
//!
//! | Expression | Meaning |
//! |------------|---------|
//! | `* *` | Every minute |
//! | `*/5 *` | Every 5 minutes |
//! | `0 *` | At the top of every hour |
//! | `0 9` | Every day at 09:00 |
//! | `30 6` | Every day at 06:30 |
//! | `*/15 8` | Every 15 minutes during the 8 o'clock hour |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use tokio::sync::mpsc;
//! use tokio_prompt_orchestrator::{PromptRequest, SessionId};
//! use tokio_prompt_orchestrator::scheduler::{Scheduler, ScheduledPrompt};
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, mut rx) = mpsc::channel::<PromptRequest>(256);
//!
//!     let scheduler = Arc::new(Scheduler::new(tx));
//!
//!     scheduler.add(ScheduledPrompt::new(
//!         "health-check",
//!         "*/5 *",
//!         "Are you operational?",
//!     )).expect("valid cron expression");
//!
//!     let handle = scheduler.spawn();
//!
//!     // The scheduler runs until the handle is dropped or abort() is called.
//!     handle.abort();
//! }
//! ```
//!
//! ## Web API integration
//!
//! When the `web-api` feature is enabled, the [`SchedulerState`] type provides
//! a thread-safe wrapper suitable for use as Axum `State`.
//! Register routes with [`scheduler_routes`] to expose
//! `POST /api/v1/schedule`, `GET /api/v1/schedule`, and
//! `DELETE /api/v1/schedule/:id`.

use crate::{OrchestratorError, PromptRequest, SessionId};
use chrono::{Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ============================================================================
// CronField — parsed single cron field (minute or hour)
// ============================================================================

/// A parsed value for a single field (minute or hour) in a cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CronField {
    /// Matches every value (`*`).
    Any,
    /// Matches every N-th value starting from 0 (`*/N`).
    Every(u32),
    /// Matches exactly this value.
    Exact(u32),
}

impl CronField {
    /// Parse a single cron field token.
    ///
    /// # Errors
    ///
    /// Returns an error string when `token` cannot be parsed.
    fn parse(token: &str, max: u32) -> Result<Self, String> {
        if token == "*" {
            return Ok(Self::Any);
        }
        if let Some(rest) = token.strip_prefix("*/") {
            let n: u32 = rest
                .parse()
                .map_err(|_| format!("invalid step value in '{token}'"))?;
            if n == 0 || n > max {
                return Err(format!("step value {n} out of range (1..={max})"));
            }
            return Ok(Self::Every(n));
        }
        let v: u32 = token
            .parse()
            .map_err(|_| format!("invalid numeric value '{token}'"))?;
        if v > max {
            return Err(format!("value {v} out of range (0..={max})"));
        }
        Ok(Self::Exact(v))
    }

    /// Return `true` if `value` matches this field.
    fn matches(&self, value: u32) -> bool {
        match self {
            Self::Any => true,
            Self::Every(n) => value % n == 0,
            Self::Exact(v) => *v == value,
        }
    }
}

// ============================================================================
// CronExpression
// ============================================================================

/// A parsed two-field mini-cron expression `"MINUTE HOUR"`.
///
/// See the [module-level documentation](self) for the supported syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpression {
    minute: CronField,
    hour: CronField,
    /// The original source string, preserved for serialisation.
    raw: String,
}

impl CronExpression {
    /// Parse a cron expression string.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] when the expression cannot
    /// be parsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::scheduler::CronExpression;
    ///
    /// assert!(CronExpression::parse("*/5 *").is_ok());
    /// assert!(CronExpression::parse("0 9").is_ok());
    /// assert!(CronExpression::parse("invalid").is_err());
    /// ```
    pub fn parse(expr: &str) -> Result<Self, OrchestratorError> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(OrchestratorError::ConfigError(format!(
                "cron expression must have exactly 2 fields (minute hour), got '{expr}'"
            )));
        }
        let minute = CronField::parse(parts[0], 59).map_err(|e| {
            OrchestratorError::ConfigError(format!("minute field: {e}"))
        })?;
        let hour = CronField::parse(parts[1], 23).map_err(|e| {
            OrchestratorError::ConfigError(format!("hour field: {e}"))
        })?;
        Ok(Self {
            minute,
            hour,
            raw: expr.to_string(),
        })
    }

    /// Return `true` if this expression matches the given `hour` and `minute`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn matches(&self, hour: u32, minute: u32) -> bool {
        self.hour.matches(hour) && self.minute.matches(minute)
    }

    /// Return the raw expression string as originally provided.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Display for CronExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for CronExpression {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for CronExpression {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        CronExpression::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// ScheduledPrompt
// ============================================================================

/// A prompt template paired with a cron schedule and target pipeline.
///
/// Created via [`ScheduledPrompt::new`] and registered with a [`Scheduler`].
/// Each `ScheduledPrompt` carries a unique `id` (UUID v4) assigned at
/// construction time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledPrompt {
    /// Unique identifier, auto-assigned at construction.
    pub id: String,
    /// Human-readable label (e.g. `"health-check"` or `"daily-summary"`).
    pub name: String,
    /// The cron expression controlling when this prompt fires.
    pub schedule: CronExpression,
    /// The prompt text template submitted to the pipeline.
    pub prompt_template: String,
    /// Optional session identifier.  When `None`, a new UUID is used per run.
    pub session_id: Option<String>,
    /// Whether this scheduled prompt is currently active.
    pub enabled: bool,
    /// Optional arbitrary metadata forwarded with each submitted request.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl ScheduledPrompt {
    /// Construct a new `ScheduledPrompt` with a generated UUID and enabled by default.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] when `cron_expr` cannot be
    /// parsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::scheduler::ScheduledPrompt;
    ///
    /// let sp = ScheduledPrompt::new("health", "*/5 *", "ping").unwrap();
    /// assert!(sp.enabled);
    /// assert!(!sp.id.is_empty());
    /// ```
    pub fn new(
        name: impl Into<String>,
        cron_expr: &str,
        prompt_template: impl Into<String>,
    ) -> Result<Self, OrchestratorError> {
        let schedule = CronExpression::parse(cron_expr)?;
        Ok(Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            schedule,
            prompt_template: prompt_template.into(),
            session_id: None,
            enabled: true,
            metadata: HashMap::new(),
        })
    }

    /// Builder: set a fixed session identifier for all runs of this prompt.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Builder: attach extra metadata to every submission.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Builder: start this prompt in a disabled state.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

// ============================================================================
// Scheduler
// ============================================================================

/// Tokio-based scheduler that fires prompt submissions on a cron-like interval.
///
/// The scheduler holds a collection of [`ScheduledPrompt`]s and a sender end
/// of the pipeline's input channel.  Call [`Scheduler::spawn`] to start the
/// background task.
///
/// The scheduler checks the current wall-clock time **once per minute** and
/// fires every matching prompt.
///
/// ## Thread Safety
///
/// `Scheduler` is `Send + Sync` and is typically shared as `Arc<Scheduler>`.
/// The internal prompt list is guarded by a [`RwLock`] so that prompts can be
/// added and removed without stopping the background task.
pub struct Scheduler {
    prompts: RwLock<Vec<ScheduledPrompt>>,
    tx: mpsc::Sender<PromptRequest>,
}

impl Scheduler {
    /// Create a new `Scheduler` that submits prompts on `tx`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(tx: mpsc::Sender<PromptRequest>) -> Self {
        Self {
            prompts: RwLock::new(Vec::new()),
            tx,
        }
    }

    /// Register a [`ScheduledPrompt`].
    ///
    /// Returns the assigned `id` (a UUID v4 string) so callers can reference
    /// it later with [`Scheduler::remove`] or through the web API.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] if the prompt is disabled
    /// and thus would never fire.  Disabled prompts *can* be added — they are
    /// accepted but will not emit requests until re-enabled (e.g. via the web API).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn add(&self, prompt: ScheduledPrompt) -> Result<String, OrchestratorError> {
        let id = prompt.id.clone();
        info!(
            id = %id,
            name = %prompt.name,
            schedule = %prompt.schedule,
            enabled = prompt.enabled,
            "registered scheduled prompt"
        );
        self.prompts.write().await.push(prompt);
        Ok(id)
    }

    /// Remove the scheduled prompt with the given `id`.
    ///
    /// Returns `true` if a prompt with that ID existed and was removed, `false`
    /// if no matching ID was found.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn remove(&self, id: &str) -> bool {
        let mut prompts = self.prompts.write().await;
        let before = prompts.len();
        prompts.retain(|p| p.id != id);
        let removed = prompts.len() < before;
        if removed {
            info!(id = %id, "removed scheduled prompt");
        } else {
            warn!(id = %id, "remove_scheduled_prompt: id not found");
        }
        removed
    }

    /// Return a snapshot of all registered prompts (including disabled ones).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn list(&self) -> Vec<ScheduledPrompt> {
        self.prompts.read().await.clone()
    }

    /// Enable or disable a scheduled prompt by ID.
    ///
    /// Returns `true` when the prompt was found and updated.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut prompts = self.prompts.write().await;
        if let Some(p) = prompts.iter_mut().find(|p| p.id == id) {
            p.enabled = enabled;
            info!(id = %id, enabled = enabled, "updated scheduled prompt state");
            true
        } else {
            false
        }
    }

    /// Spawn the background scheduler task and return its [`JoinHandle`].
    ///
    /// The task loops forever, sleeping until the next minute boundary, then
    /// submitting all matching prompts.  It stops when the handle is dropped
    /// or [`JoinHandle::abort`] is called.
    ///
    /// # Panics
    ///
    /// The spawned task does not panic; any submission errors are logged and
    /// skipped.
    pub fn spawn(self: &Arc<Self>) -> JoinHandle<()> {
        let scheduler = Arc::clone(self);
        tokio::spawn(async move {
            info!("Scheduler started");
            loop {
                // Sleep until the start of the next minute.
                let now = Local::now();
                let secs_into_minute = now.second() as u64;
                let nanos_remaining = now.nanosecond() as u64;
                let sleep_ms = (60 - secs_into_minute) * 1000
                    + if nanos_remaining > 0 { 1 } else { 0 };

                debug!(sleep_ms = sleep_ms, "scheduler sleeping until next minute");
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;

                // Snapshot the current time for matching.
                let fire_time = Local::now();
                let hour = fire_time.hour();
                let minute = fire_time.minute();

                debug!(hour = hour, minute = minute, "scheduler tick");

                let prompts = scheduler.prompts.read().await;
                for prompt in prompts.iter() {
                    if !prompt.enabled {
                        continue;
                    }
                    if !prompt.schedule.matches(hour, minute) {
                        continue;
                    }
                    let session = prompt
                        .session_id
                        .clone()
                        .unwrap_or_else(|| Uuid::new_v4().to_string());
                    let request_id = Uuid::new_v4().to_string();
                    let req = PromptRequest {
                        session: SessionId::new(session),
                        request_id: request_id.clone(),
                        input: prompt.prompt_template.clone(),
                        meta: prompt.metadata.clone(),
                        deadline: None,
                    };
                    match scheduler.tx.try_send(req) {
                        Ok(_) => {
                            info!(
                                scheduled_prompt = %prompt.name,
                                request_id = %request_id,
                                "submitted scheduled prompt"
                            );
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            warn!(
                                scheduled_prompt = %prompt.name,
                                "pipeline input channel full; skipping scheduled prompt"
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            error!("pipeline input channel closed; stopping scheduler");
                            return;
                        }
                    }
                }
            }
        })
    }
}

// ============================================================================
// SchedulerState — web API integration
// ============================================================================

/// Thread-safe wrapper around a [`Scheduler`] for use as Axum `State`.
///
/// Exposes the scheduler over the web API when the `web-api` feature is enabled.
/// Routes are registered via [`scheduler_routes`].
///
/// ## Endpoints (registered by `scheduler_routes`)
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `POST` | `/api/v1/schedule` | Register a new scheduled prompt |
/// | `GET` | `/api/v1/schedule` | List all scheduled prompts |
/// | `DELETE` | `/api/v1/schedule/:id` | Remove a scheduled prompt |
/// | `PATCH` | `/api/v1/schedule/:id/enable` | Enable a prompt |
/// | `PATCH` | `/api/v1/schedule/:id/disable` | Disable a prompt |
#[derive(Clone)]
pub struct SchedulerState {
    pub scheduler: Arc<Scheduler>,
}

impl SchedulerState {
    /// Wrap an existing [`Scheduler`] in a `SchedulerState`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(scheduler: Arc<Scheduler>) -> Self {
        Self { scheduler }
    }
}

// ============================================================================
// Web API handlers (feature-gated)
// ============================================================================

/// Axum request body for `POST /api/v1/schedule`.
#[cfg(feature = "web-api")]
#[derive(Debug, Deserialize)]
pub struct CreateScheduleRequest {
    /// Human-readable name.
    pub name: String,
    /// Cron expression, e.g. `"*/5 *"`.
    pub schedule: String,
    /// The prompt template to submit on each firing.
    pub prompt_template: String,
    /// Optional session ID.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Whether to start enabled.  Default: `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[cfg(feature = "web-api")]
fn default_enabled() -> bool {
    true
}

/// Axum response body for schedule endpoints.
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
pub struct ScheduleResponse {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub enabled: bool,
    pub prompt_preview: String,
}

#[cfg(feature = "web-api")]
impl From<&ScheduledPrompt> for ScheduleResponse {
    fn from(p: &ScheduledPrompt) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            schedule: p.schedule.to_string(),
            enabled: p.enabled,
            prompt_preview: p.prompt_template.chars().take(80).collect(),
        }
    }
}

/// Build an Axum [`Router`] with all scheduler endpoints.
///
/// Merge this router into your application router:
///
/// ```rust,no_run
/// # #[cfg(feature = "web-api")]
/// # {
/// use axum::Router;
/// use tokio_prompt_orchestrator::scheduler::{Scheduler, SchedulerState, scheduler_routes};
/// use std::sync::Arc;
/// use tokio::sync::mpsc;
///
/// let (tx, _rx) = mpsc::channel(256);
/// let scheduler = Arc::new(Scheduler::new(tx));
/// let state = SchedulerState::new(scheduler);
///
/// let app: Router = Router::new()
///     .merge(scheduler_routes(state));
/// # }
/// ```
#[cfg(feature = "web-api")]
pub fn scheduler_routes(state: SchedulerState) -> axum::Router {
    use axum::routing::{delete, get, patch, post};

    axum::Router::new()
        .route("/api/v1/schedule", post(create_schedule_handler))
        .route("/api/v1/schedule", get(list_schedules_handler))
        .route("/api/v1/schedule/:id", delete(delete_schedule_handler))
        .route("/api/v1/schedule/:id/enable", patch(enable_schedule_handler))
        .route(
            "/api/v1/schedule/:id/disable",
            patch(disable_schedule_handler),
        )
        .with_state(state)
}

/// `POST /api/v1/schedule` — register a new scheduled prompt.
#[cfg(feature = "web-api")]
async fn create_schedule_handler(
    axum::extract::State(state): axum::extract::State<SchedulerState>,
    axum::Json(body): axum::Json<CreateScheduleRequest>,
) -> axum::response::Response {
    use axum::{
        http::StatusCode,
        response::IntoResponse,
        Json,
    };

    let mut prompt = match ScheduledPrompt::new(&body.name, &body.schedule, &body.prompt_template) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    prompt.session_id = body.session_id;
    prompt.metadata = body.metadata;
    if !body.enabled {
        prompt.enabled = false;
    }

    let resp = ScheduleResponse::from(&prompt);

    match state.scheduler.add(prompt).await {
        Ok(_) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/schedule` — list all scheduled prompts.
#[cfg(feature = "web-api")]
async fn list_schedules_handler(
    axum::extract::State(state): axum::extract::State<SchedulerState>,
) -> axum::Json<Vec<ScheduleResponse>> {
    let prompts = state.scheduler.list().await;
    axum::Json(prompts.iter().map(ScheduleResponse::from).collect())
}

/// `DELETE /api/v1/schedule/:id` — remove a scheduled prompt.
#[cfg(feature = "web-api")]
async fn delete_schedule_handler(
    axum::extract::State(state): axum::extract::State<SchedulerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};

    if state.scheduler.remove(&id).await {
        (StatusCode::OK, Json(serde_json::json!({"deleted": id}))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "scheduled prompt not found", "id": id})),
        )
            .into_response()
    }
}

/// `PATCH /api/v1/schedule/:id/enable` — enable a scheduled prompt.
#[cfg(feature = "web-api")]
async fn enable_schedule_handler(
    axum::extract::State(state): axum::extract::State<SchedulerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    toggle_enabled(state, id, true).await
}

/// `PATCH /api/v1/schedule/:id/disable` — disable a scheduled prompt.
#[cfg(feature = "web-api")]
async fn disable_schedule_handler(
    axum::extract::State(state): axum::extract::State<SchedulerState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    toggle_enabled(state, id, false).await
}

#[cfg(feature = "web-api")]
async fn toggle_enabled(
    state: SchedulerState,
    id: String,
    enabled: bool,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};

    if state.scheduler.set_enabled(&id, enabled).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({"id": id, "enabled": enabled})),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "scheduled prompt not found", "id": id})),
        )
            .into_response()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_field_parse_any() {
        let f = CronField::parse("*", 59).expect("should parse");
        assert_eq!(f, CronField::Any);
        assert!(f.matches(0));
        assert!(f.matches(30));
        assert!(f.matches(59));
    }

    #[test]
    fn cron_field_parse_every() {
        let f = CronField::parse("*/5", 59).expect("should parse");
        assert_eq!(f, CronField::Every(5));
        assert!(f.matches(0));
        assert!(f.matches(5));
        assert!(f.matches(15));
        assert!(!f.matches(7));
    }

    #[test]
    fn cron_field_parse_exact() {
        let f = CronField::parse("30", 59).expect("should parse");
        assert_eq!(f, CronField::Exact(30));
        assert!(f.matches(30));
        assert!(!f.matches(31));
    }

    #[test]
    fn cron_field_invalid_step_zero() {
        assert!(CronField::parse("*/0", 59).is_err());
    }

    #[test]
    fn cron_field_out_of_range() {
        assert!(CronField::parse("60", 59).is_err());
    }

    #[test]
    fn cron_expression_parse_valid() {
        let expr = CronExpression::parse("*/5 *").expect("valid");
        assert!(expr.matches(0, 0));
        assert!(expr.matches(0, 5));
        assert!(!expr.matches(0, 3));
    }

    #[test]
    fn cron_expression_parse_exact_hour_minute() {
        let expr = CronExpression::parse("30 9").expect("valid");
        assert!(expr.matches(9, 30));
        assert!(!expr.matches(9, 31));
        assert!(!expr.matches(10, 30));
    }

    #[test]
    fn cron_expression_every_minute() {
        let expr = CronExpression::parse("* *").expect("valid");
        assert!(expr.matches(0, 0));
        assert!(expr.matches(23, 59));
    }

    #[test]
    fn cron_expression_wrong_field_count() {
        assert!(CronExpression::parse("*").is_err());
        assert!(CronExpression::parse("* * *").is_err());
    }

    #[test]
    fn scheduled_prompt_new() {
        let sp = ScheduledPrompt::new("test", "*/10 *", "hello").expect("valid");
        assert!(sp.enabled);
        assert_eq!(sp.name, "test");
        assert_eq!(sp.prompt_template, "hello");
        assert!(!sp.id.is_empty());
    }

    #[test]
    fn scheduled_prompt_builders() {
        let sp = ScheduledPrompt::new("x", "0 9", "ping")
            .expect("valid")
            .with_session("my-session")
            .with_metadata("env", "prod")
            .disabled();
        assert_eq!(sp.session_id.as_deref(), Some("my-session"));
        assert_eq!(sp.metadata.get("env").map(String::as_str), Some("prod"));
        assert!(!sp.enabled);
    }

    #[tokio::test]
    async fn scheduler_add_list_remove() {
        let (tx, _rx) = mpsc::channel(16);
        let scheduler = Arc::new(Scheduler::new(tx));

        let sp = ScheduledPrompt::new("test", "* *", "hello").expect("valid");
        let id = scheduler.add(sp).await.expect("added");

        let list = scheduler.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);

        let removed = scheduler.remove(&id).await;
        assert!(removed);

        assert!(scheduler.list().await.is_empty());
    }

    #[tokio::test]
    async fn scheduler_set_enabled() {
        let (tx, _rx) = mpsc::channel(16);
        let scheduler = Arc::new(Scheduler::new(tx));

        let sp = ScheduledPrompt::new("test", "* *", "hello").expect("valid");
        let id = scheduler.add(sp).await.expect("added");

        assert!(scheduler.set_enabled(&id, false).await);
        let list = scheduler.list().await;
        assert!(!list[0].enabled);

        assert!(scheduler.set_enabled(&id, true).await);
        let list = scheduler.list().await;
        assert!(list[0].enabled);
    }

    #[test]
    fn cron_expression_serialises_to_string() {
        let expr = CronExpression::parse("*/15 8").expect("valid");
        let json = serde_json::to_string(&expr).expect("serialise");
        assert_eq!(json, "\"*/15 8\"");
    }

    #[test]
    fn cron_expression_deserialises_from_string() {
        let expr: CronExpression = serde_json::from_str("\"0 9\"").expect("deserialise");
        assert!(expr.matches(9, 0));
    }
}
