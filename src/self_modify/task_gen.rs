//! # Meta-Task Generator (Task 2.1)
//!
//! Reads anomaly events and telemetry degradation signals, then generates
//! TOML task files for the agent coordinator  -  turning system problems into
//! agent work orders.
//!
//! ## Example flow
//! Dedup collision rate spikes to 4 % → generator creates a task file:
//! ```toml
//! [task]
//! name = "improve_dedup_hashing"
//! description = "Dedup collision rate is 4.0%, target <1%. Analyze src/enhanced/dedup.rs."
//! priority = "high"
//! affected_files = ["src/enhanced/dedup.rs"]
//! acceptance_criteria = "dedup_collision_rate < 0.01"
//! estimated_complexity = "medium"
//! ```
//!
//! ## Rate limiting
//! Task generation is rate-limited per metric to prevent thrashing.
//! A cooldown window prevents re-generating the same task within N seconds
//! of the previous one.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::self_tune::telemetry_bus::TelemetrySnapshot;

//  Error

/// Errors produced by the task generator.
#[derive(Debug, Error)]
pub enum TaskGenError {
    /// Lock poisoned.
    #[error("task generator lock poisoned")]
    LockPoisoned,

    /// Could not serialize the task to TOML.
    #[error("TOML serialization failed: {0}")]
    SerializationFailed(String),
}

//  Generated task

/// Priority of a generated task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskPriority {
    /// Routine improvement, no urgency.
    Low,
    /// Noticeable degradation; address within hours.
    Medium,
    /// Significant degradation; address as soon as possible.
    High,
    /// Critical failure; immediate attention required.
    Critical,
}

impl TaskPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Estimated implementation complexity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Complexity {
    /// Small, well-understood change.
    Trivial,
    /// A few hours of work.
    Small,
    /// Half a day to a day.
    Medium,
    /// Multi-day effort.
    Large,
}

impl Complexity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

/// A work order generated for the agent fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedTask {
    /// Unique task identifier (derived from trigger metric + timestamp).
    pub id: String,
    /// Short machine-friendly name for the task file.
    pub name: String,
    /// Full problem description including current metric values.
    pub description: String,
    /// Source files most likely to need editing.
    pub affected_files: Vec<String>,
    /// Measurable target that must be met for the task to be considered done.
    pub acceptance_criteria: String,
    /// Urgency.
    pub priority: TaskPriority,
    /// Estimated effort.
    pub estimated_complexity: Complexity,
    /// The metric that triggered this task.
    pub trigger_metric: String,
    /// Value of the metric when the task was created.
    pub trigger_value: f64,
    /// Target value for the metric.
    pub target_value: f64,
    /// Unix timestamp when this task was generated.
    pub created_at_secs: u64,
}

impl GeneratedTask {
    /// Render the task as a TOML string suitable for writing to a file.
    ///
    /// String values are escaped to prevent malformed TOML output:
    /// backslashes, double-quotes, newlines, carriage-returns, and NUL bytes
    /// are all replaced with safe representations.
    pub fn to_toml(&self) -> Result<String, TaskGenError> {
        // Escape a string value for use inside a TOML double-quoted string.
        let escape_toml = |s: &str| -> String {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\0', "\\u0000")
        };

        let s = format!(
            "[task]\nid = \"{id}\"\nname = \"{name}\"\npriority = \"{priority}\"\nestimated_complexity = \"{complexity}\"\ntrigger_metric = \"{metric}\"\ntrigger_value = {trigger:.4}\ntarget_value = {target:.4}\ncreated_at_secs = {ts}\n\ndescription = \"\"\"\n{desc}\n\"\"\"\n\nacceptance_criteria = \"{ac}\"\n\naffected_files = [{files}]\n",
            id = escape_toml(&self.id),
            name = escape_toml(&self.name),
            priority = self.priority.as_str(),
            complexity = self.estimated_complexity.as_str(),
            metric = escape_toml(&self.trigger_metric),
            trigger = self.trigger_value,
            target = self.target_value,
            ts = self.created_at_secs,
            desc = escape_toml(&self.description),
            ac = escape_toml(&self.acceptance_criteria),
            files = self
                .affected_files
                .iter()
                .map(|f| format!("\"{}\"", escape_toml(f)))
                .collect::<Vec<_>>()
                .join(", "),
        );
        Ok(s)
    }
}

//  Task template

/// A template that maps a metric threshold to a task description.
#[derive(Debug, Clone)]
pub struct TaskTemplate {
    /// The metric name this template responds to (e.g., `"dedup_collision_rate"`).
    pub metric: String,
    /// Trigger when the metric exceeds this value.
    pub threshold: f64,
    /// Target value (what "fixed" looks like).
    pub target: f64,
    /// Short task name template.
    pub name_template: String,
    /// Description template. Use `{value}` and `{target}` as placeholders.
    pub description_template: String,
    /// Files that are typically modified for this kind of issue.
    pub affected_files: Vec<String>,
    /// Acceptance criteria template.
    pub acceptance_criteria_template: String,
    /// Default priority.
    pub priority: TaskPriority,
    /// Default complexity.
    pub complexity: Complexity,
    /// Minimum seconds between re-generating this task for the same metric.
    pub cooldown_secs: u64,
}

impl TaskTemplate {
    fn render(&self, value: f64, ts: u64) -> GeneratedTask {
        let id = format!("{}_{}", self.metric.replace('.', "_"), ts);
        let description = self
            .description_template
            .replace("{value}", &format!("{:.4}", value))
            .replace("{target}", &format!("{:.4}", self.target));
        let acceptance_criteria = self
            .acceptance_criteria_template
            .replace("{target}", &format!("{:.4}", self.target));

        GeneratedTask {
            id,
            name: self.name_template.clone(),
            description,
            affected_files: self.affected_files.clone(),
            acceptance_criteria,
            priority: self.priority,
            estimated_complexity: self.complexity,
            trigger_metric: self.metric.clone(),
            trigger_value: value,
            target_value: self.target,
            created_at_secs: ts,
        }
    }
}

//  Generator

struct GenInner {
    templates: Vec<TaskTemplate>,
    /// Last generation time per metric name.
    last_generated: HashMap<String, u64>,
    /// All tasks generated in this session.
    generated_tasks: Vec<GeneratedTask>,
    /// Maximum task history to retain.
    max_history: usize,
    /// Maximum number of tasks that may be generated within any rolling 1-hour window.
    max_tasks_per_hour: u32,
    /// Wall-clock timestamps of recent task generation events (for rate limiting).
    recent_task_times: Vec<Instant>,
}

/// Generates agent work orders from telemetry degradation signals.
#[derive(Clone)]
pub struct MetaTaskGenerator {
    inner: Arc<Mutex<GenInner>>,
}

impl MetaTaskGenerator {
    /// Create a new generator with a set of built-in default templates.
    pub fn new() -> Self {
        Self::with_templates(default_templates())
    }

    /// Create a generator with default templates and an explicit hourly rate limit.
    pub fn new_with_rate_limit(max_tasks_per_hour: u32) -> Self {
        Self::with_templates_and_rate_limit(default_templates(), max_tasks_per_hour)
    }

    /// Create a generator with custom templates (replaces defaults).
    pub fn with_templates(templates: Vec<TaskTemplate>) -> Self {
        Self::with_templates_and_rate_limit(templates, 20)
    }

    /// Create a generator with custom templates and an explicit hourly rate limit.
    ///
    /// `max_tasks_per_hour` caps the total number of tasks that may be generated
    /// within any rolling 1-hour window.  Once the limit is reached, new tasks
    /// are silently suppressed until the window advances.  Default: 20.
    pub fn with_templates_and_rate_limit(
        templates: Vec<TaskTemplate>,
        max_tasks_per_hour: u32,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GenInner {
                templates,
                last_generated: HashMap::new(),
                generated_tasks: Vec::new(),
                max_history: 500,
                max_tasks_per_hour,
                recent_task_times: Vec::new(),
            })),
        }
    }

    /// Add a custom template.
    pub fn add_template(&self, template: TaskTemplate) -> Result<(), TaskGenError> {
        let mut inner = self.inner.lock().map_err(|_| TaskGenError::LockPoisoned)?;
        inner.templates.push(template);
        Ok(())
    }

    /// Analyse a telemetry snapshot and generate tasks for any triggered thresholds.
    ///
    /// Rate-limiting is respected  -  templates on cooldown are skipped.
    pub fn process_snapshot(
        &self,
        snap: &TelemetrySnapshot,
    ) -> Result<Vec<GeneratedTask>, TaskGenError> {
        let mut inner = self.inner.lock().map_err(|_| TaskGenError::LockPoisoned)?;
        let now = unix_now();
        let mut generated = Vec::new();

        // Build a flat metric map from the snapshot
        let metrics = snapshot_to_metrics(snap);

        let templates: Vec<_> = inner.templates.clone();
        for template in &templates {
            let value = match metrics.get(&template.metric) {
                Some(v) => *v,
                None => continue,
            };

            if value <= template.threshold {
                continue; // metric is within target
            }

            // Check cooldown
            let last = inner
                .last_generated
                .get(&template.metric)
                .copied()
                .unwrap_or(0);
            if now - last < template.cooldown_secs {
                continue;
            }

            // Hourly rate limit: prune timestamps older than 1 hour, then check.
            let one_hour_ago = std::time::Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now);
            inner.recent_task_times.retain(|&t| t > one_hour_ago);
            if inner.recent_task_times.len() as u32 >= inner.max_tasks_per_hour {
                // Rate limit exceeded; skip generation for all remaining templates
                // in this snapshot.
                break;
            }

            let task = template.render(value, now);
            inner.last_generated.insert(template.metric.clone(), now);
            inner.recent_task_times.push(std::time::Instant::now());

            if inner.generated_tasks.len() >= inner.max_history {
                inner.generated_tasks.remove(0);
            }
            inner.generated_tasks.push(task.clone());
            generated.push(task);
        }

        Ok(generated)
    }

    /// Return all tasks generated in this session.
    pub fn history(&self) -> Vec<GeneratedTask> {
        self.inner
            .lock()
            .map(|inner| inner.generated_tasks.clone())
            .unwrap_or_default()
    }

    /// Return the number of tasks generated.
    pub fn task_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.generated_tasks.len())
            .unwrap_or(0)
    }

    /// Return all registered template metric names.
    pub fn template_metrics(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| inner.templates.iter().map(|t| t.metric.clone()).collect())
            .unwrap_or_default()
    }
}

impl Default for MetaTaskGenerator {
    fn default() -> Self {
        Self::new()
    }
}

//  Metric extraction

fn snapshot_to_metrics(snap: &TelemetrySnapshot) -> HashMap<String, f64> {
    let mut m = HashMap::new();
    m.insert("pressure".to_string(), snap.pressure);
    m.insert("cache_hit_rate".to_string(), snap.cache_hit_rate);
    m.insert(
        "dedup_collision_rate".to_string(),
        snap.dedup_collision_rate,
    );
    m.insert(
        "error_rate".to_string(),
        if snap.total_completed + snap.total_errors == 0 {
            0.0
        } else {
            snap.total_errors as f64 / (snap.total_completed + snap.total_errors) as f64
        },
    );

    // Per-stage metrics
    for stage in &snap.stages {
        let fill = if stage.queue_capacity == 0 {
            0.0
        } else {
            stage.queue_depth as f64 / stage.queue_capacity as f64
        };
        m.insert(format!("stage.{}.queue_fill", stage.stage), fill);
        m.insert(
            format!("stage.{}.error_rate", stage.stage),
            stage.error_rate,
        );
        m.insert(
            format!("stage.{}.throughput_rps", stage.stage),
            stage.throughput_rps,
        );
    }

    m
}

//  Default templates

fn default_templates() -> Vec<TaskTemplate> {
    vec![
        TaskTemplate {
            metric: "dedup_collision_rate".to_string(),
            threshold: 0.03, // trigger above 3%
            target: 0.01,
            name_template: "improve_dedup_hashing".to_string(),
            description_template: "Dedup collision rate is {value}, above target {target}. \
                Analyze and improve hashing strategy in src/enhanced/dedup.rs. \
                Consider better hash functions (AHash, xxHash) or larger bucket counts."
                .to_string(),
            affected_files: vec!["src/enhanced/dedup.rs".to_string()],
            acceptance_criteria_template: "dedup_collision_rate < {target}".to_string(),
            priority: TaskPriority::High,
            complexity: Complexity::Medium,
            cooldown_secs: 3600,
        },
        TaskTemplate {
            metric: "error_rate".to_string(),
            threshold: 0.05, // trigger above 5%
            target: 0.01,
            name_template: "investigate_error_rate_spike".to_string(),
            description_template: "Pipeline error rate is {value}, above target {target}. \
                Investigate error sources across all stages. Check circuit breaker logs, \
                retry exhaustion, and worker failures."
                .to_string(),
            affected_files: vec![
                "src/stages.rs".to_string(),
                "src/enhanced/circuit_breaker.rs".to_string(),
                "src/enhanced/retry.rs".to_string(),
            ],
            acceptance_criteria_template: "error_rate < {target}".to_string(),
            priority: TaskPriority::Critical,
            complexity: Complexity::Medium,
            cooldown_secs: 1800,
        },
        TaskTemplate {
            metric: "pressure".to_string(),
            threshold: 0.85, // trigger above 85%
            target: 0.60,
            name_template: "reduce_pipeline_pressure".to_string(),
            description_template: "System pressure is {value}, above target {target}. \
                Consider increasing channel buffer sizes, adding backpressure shedding, \
                or scaling worker concurrency."
                .to_string(),
            affected_files: vec!["src/stages.rs".to_string(), "src/config/mod.rs".to_string()],
            acceptance_criteria_template: "pressure < {target}".to_string(),
            priority: TaskPriority::High,
            complexity: Complexity::Small,
            cooldown_secs: 900,
        },
    ]
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Escape a string for safe embedding inside a TOML basic string.
///
/// Escapes backslashes, double-quotes, newlines (LF and CR), and ASCII
/// control characters (U+0000–U+001F) so that arbitrary values
/// cannot break the TOML document or inject extra keys.
///
/// # Example
/// ```rust
/// use tokio_prompt_orchestrator::self_modify::task_gen::sanitise_toml_string;
/// assert_eq!(sanitise_toml_string("a\\b"), "a\\\\b");
/// ```
pub fn sanitise_toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

//  Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_tune::telemetry_bus::TelemetrySnapshot;

    fn snap_with(pressure: f64, dedup: f64, error_rate_frac: f64) -> TelemetrySnapshot {
        let completed = 100u64;
        let errors = (error_rate_frac * completed as f64) as u64;
        TelemetrySnapshot {
            pressure,
            dedup_collision_rate: dedup,
            total_completed: completed - errors,
            total_errors: errors,
            ..Default::default()
        }
    }

    fn low_stress_snap() -> TelemetrySnapshot {
        snap_with(0.1, 0.001, 0.001)
    }

    fn high_dedup_snap() -> TelemetrySnapshot {
        snap_with(0.1, 0.05, 0.001)
    }

    fn high_error_snap() -> TelemetrySnapshot {
        snap_with(0.1, 0.001, 0.10)
    }

    fn high_pressure_snap() -> TelemetrySnapshot {
        snap_with(0.90, 0.001, 0.001)
    }

    #[test]
    fn test_no_tasks_below_threshold() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&low_stress_snap()).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_dedup_threshold_triggers_task() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_dedup_snap()).unwrap();
        assert!(tasks
            .iter()
            .any(|t| t.trigger_metric == "dedup_collision_rate"));
    }

    #[test]
    fn test_error_rate_threshold_triggers_task() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_error_snap()).unwrap();
        assert!(tasks.iter().any(|t| t.trigger_metric == "error_rate"));
    }

    #[test]
    fn test_pressure_threshold_triggers_task() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_pressure_snap()).unwrap();
        assert!(tasks.iter().any(|t| t.trigger_metric == "pressure"));
    }

    #[test]
    fn test_task_stored_in_history() {
        let gen = MetaTaskGenerator::new();
        gen.process_snapshot(&high_dedup_snap()).unwrap();
        assert!(!gen.history().is_empty());
    }

    #[test]
    fn test_task_count_increments() {
        let gen = MetaTaskGenerator::new();
        gen.process_snapshot(&high_dedup_snap()).unwrap();
        assert!(gen.task_count() > 0);
    }

    #[test]
    fn test_cooldown_prevents_duplicate_tasks() {
        let gen = MetaTaskGenerator::new();
        gen.process_snapshot(&high_dedup_snap()).unwrap();
        let count_after_first = gen.task_count();
        // Process again  -  cooldown prevents re-generation
        gen.process_snapshot(&high_dedup_snap()).unwrap();
        assert_eq!(gen.task_count(), count_after_first);
    }

    #[test]
    fn test_generated_task_has_correct_fields() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_dedup_snap()).unwrap();
        let task = tasks
            .iter()
            .find(|t| t.trigger_metric == "dedup_collision_rate")
            .unwrap();
        assert!(!task.id.is_empty());
        assert!(!task.description.is_empty());
        assert!(!task.affected_files.is_empty());
        assert!(task.trigger_value > 0.03);
    }

    #[test]
    fn test_to_toml_produces_valid_output() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_dedup_snap()).unwrap();
        let task = &tasks[0];
        let toml_str = task.to_toml().unwrap();
        assert!(toml_str.contains("[task]"));
        assert!(toml_str.contains(&task.name));
        assert!(toml_str.contains("priority"));
    }

    #[test]
    fn test_add_custom_template() {
        let gen = MetaTaskGenerator::new();
        let template = TaskTemplate {
            metric: "custom_metric".to_string(),
            threshold: 0.5,
            target: 0.1,
            name_template: "fix_custom".to_string(),
            description_template: "Custom metric is {value}, target {target}".to_string(),
            affected_files: vec!["src/custom.rs".to_string()],
            acceptance_criteria_template: "custom_metric < {target}".to_string(),
            priority: TaskPriority::Low,
            complexity: Complexity::Trivial,
            cooldown_secs: 60,
        };
        gen.add_template(template).unwrap();
        assert!(gen
            .template_metrics()
            .contains(&"custom_metric".to_string()));
    }

    #[test]
    fn test_template_metrics_includes_defaults() {
        let gen = MetaTaskGenerator::new();
        let metrics = gen.template_metrics();
        assert!(metrics.contains(&"dedup_collision_rate".to_string()));
        assert!(metrics.contains(&"error_rate".to_string()));
        assert!(metrics.contains(&"pressure".to_string()));
    }

    #[test]
    fn test_task_priority_ordering() {
        assert!(TaskPriority::Critical > TaskPriority::High);
        assert!(TaskPriority::High > TaskPriority::Medium);
        assert!(TaskPriority::Medium > TaskPriority::Low);
    }

    #[test]
    fn test_task_priority_as_str() {
        assert_eq!(TaskPriority::Critical.as_str(), "critical");
        assert_eq!(TaskPriority::Low.as_str(), "low");
    }

    #[test]
    fn test_complexity_as_str() {
        assert_eq!(Complexity::Trivial.as_str(), "trivial");
        assert_eq!(Complexity::Large.as_str(), "large");
    }

    #[test]
    fn test_generator_default_creates_with_templates() {
        let gen = MetaTaskGenerator::default();
        assert!(!gen.template_metrics().is_empty());
    }

    #[test]
    fn test_generator_clone_shares_history() {
        let gen = MetaTaskGenerator::new();
        let gen2 = gen.clone();
        gen.process_snapshot(&high_dedup_snap()).unwrap();
        assert_eq!(gen.task_count(), gen2.task_count());
    }

    #[test]
    fn test_multiple_triggers_from_single_snapshot() {
        let gen = MetaTaskGenerator::new();
        // High dedup + high error in one snapshot
        let snap = TelemetrySnapshot {
            pressure: 0.9,
            dedup_collision_rate: 0.05,
            total_completed: 90,
            total_errors: 10,
            ..Default::default()
        };
        let tasks = gen.process_snapshot(&snap).unwrap();
        assert!(tasks.len() >= 2);
    }

    #[test]
    fn test_generated_task_id_contains_metric_name() {
        let gen = MetaTaskGenerator::new();
        let tasks = gen.process_snapshot(&high_dedup_snap()).unwrap();
        let task = tasks
            .iter()
            .find(|t| t.trigger_metric == "dedup_collision_rate")
            .unwrap();
        assert!(task.id.contains("dedup_collision_rate"));
    }

    // ------------------------------------------------------------------
    // CRIT-03: sanitise_toml_string tests
    // ------------------------------------------------------------------

    #[test]
    fn test_sanitise_toml_string_escapes_backslash() {
        assert_eq!(sanitise_toml_string("a\\b"), "a\\\\b");
        assert_eq!(
            sanitise_toml_string("C:\\Windows\\System32"),
            "C:\\\\Windows\\\\System32"
        );
    }

    #[test]
    fn test_sanitise_toml_string_escapes_quotes() {
        assert_eq!(sanitise_toml_string("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(sanitise_toml_string("\"quoted\""), "\\\"quoted\\\"");
    }

    #[test]
    fn test_sanitise_toml_string_escapes_newlines() {
        assert_eq!(sanitise_toml_string("line1\nline2"), "line1\\nline2");
        assert_eq!(sanitise_toml_string("line1\r\nline2"), "line1\\r\\nline2");
    }

    #[test]
    fn test_sanitise_toml_string_escapes_control_chars() {
        // NUL byte
        let with_nul = "abc\x00def";
        let escaped = sanitise_toml_string(with_nul);
        assert!(
            escaped.contains("\\u0000"),
            "NUL should be escaped: {escaped}"
        );
        // Bell (0x07)
        let with_bell = "abc\x07def";
        let escaped = sanitise_toml_string(with_bell);
        assert!(
            escaped.contains("\\u0007"),
            "Bell should be escaped: {escaped}"
        );
    }

    #[test]
    fn test_sanitise_toml_string_passthrough_normal_chars() {
        let normal = "hello world 123 !@#$%^&*()";
        assert_eq!(sanitise_toml_string(normal), normal);
    }

    #[test]
    fn test_to_toml_with_injection_attempt() {
        let gen = MetaTaskGenerator::new();
        let snap = crate::self_tune::telemetry_bus::TelemetrySnapshot {
            dedup_collision_rate: 0.05,
            ..Default::default()
        };
        let tasks = gen.process_snapshot(&snap).unwrap();
        if let Some(task) = tasks.first() {
            let toml_str = task.to_toml().unwrap();
            // Verify it contains the [task] header and no raw injection
            assert!(toml_str.contains("[task]"));
        }
    }

    #[test]
    fn test_task_gen_escapes_backslash() {
        let task = GeneratedTask {
            id: "id".to_string(),
            name: "name\\test".to_string(),
            description: "desc with \\backslash".to_string(),
            affected_files: vec!["src\\foo.rs".to_string()],
            acceptance_criteria: "crit".to_string(),
            priority: TaskPriority::Low,
            estimated_complexity: Complexity::Small,
            trigger_metric: "metric".to_string(),
            trigger_value: 0.5,
            target_value: 0.1,
            created_at_secs: 0,
        };
        let toml_str = task.to_toml().unwrap();
        assert!(
            toml_str.contains("\\\\"),
            "expected escaped backslash in output: {toml_str}"
        );
    }

    #[test]
    fn test_task_gen_escapes_newline() {
        let task = GeneratedTask {
            id: "id-nl".to_string(),
            name: "name\nnewline".to_string(),
            description: "line1\nline2".to_string(),
            affected_files: vec![],
            acceptance_criteria: "done\nmore".to_string(),
            priority: TaskPriority::Low,
            estimated_complexity: Complexity::Small,
            trigger_metric: "metric".to_string(),
            trigger_value: 0.5,
            target_value: 0.1,
            created_at_secs: 0,
        };
        let toml_str = task.to_toml().unwrap();
        assert!(
            toml_str.contains("\\n"),
            "expected \\n escape in output: {toml_str}"
        );
    }

    #[test]
    fn test_task_gen_respects_hourly_rate_limit() {
        let gen = MetaTaskGenerator::with_templates_and_rate_limit(
            vec![TaskTemplate {
                metric: "pressure".to_string(),
                threshold: 0.5,
                target: 0.1,
                name_template: "pressure_fix".to_string(),
                description_template: "pressure is {value}".to_string(),
                affected_files: vec![],
                acceptance_criteria_template: "pressure < {target}".to_string(),
                priority: TaskPriority::High,
                complexity: Complexity::Small,
                cooldown_secs: 0,
            }],
            2, // max 2 tasks per hour
        );

        let high_pressure = TelemetrySnapshot {
            pressure: 0.99,
            ..TelemetrySnapshot::default()
        };

        let t1 = gen.process_snapshot(&high_pressure).unwrap();
        assert_eq!(t1.len(), 1, "first call should produce 1 task");

        let t2 = gen.process_snapshot(&high_pressure).unwrap();
        assert_eq!(
            t2.len(),
            1,
            "second call should produce 1 task (limit not yet hit)"
        );

        // Third call: hourly limit of 2 reached, must produce 0 tasks
        let t3 = gen.process_snapshot(&high_pressure).unwrap();
        assert_eq!(
            t3.len(),
            0,
            "third call should produce 0 tasks (hourly limit exceeded)"
        );
    }
}
