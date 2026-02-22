//! # Self-Documentation (Task 2.5)
//!
//! Every time the system modifies itself, auto-generate:
//! - Changelog entries with what changed and why
//! - Updated architecture diagrams (Mermaid)
//! - Performance delta reports
//! - Dependency impact analysis
//!
//! Output is written to `docs/self_modifications/` as markdown files.
//! The system can explain its own evolution through these records.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Error ────────────────────────────────────────────────────────────────────

/// Errors from the documentation generator.
#[derive(Debug, Error)]
pub enum DocsError {
    /// Lock poisoned.
    #[error("docs lock poisoned")]
    LockPoisoned,

    /// I/O error writing a document.
    #[error("I/O error: {0}")]
    Io(String),
}

// ─── Document types ───────────────────────────────────────────────────────────

/// A metric value before and after a modification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricDelta {
    /// Metric name (e.g., `"p95_latency_ms"`)
    pub name: String,
    /// Value before the change.
    pub before: f64,
    /// Value after the change.
    pub after: f64,
    /// Computed delta (`after - before`).
    pub delta: f64,
    /// Whether a lower value is better for this metric.
    pub lower_is_better: bool,
}

impl MetricDelta {
    /// Create a new delta record.
    pub fn new(name: impl Into<String>, before: f64, after: f64, lower_is_better: bool) -> Self {
        let delta = after - before;
        Self {
            name: name.into(),
            before,
            after,
            delta,
            lower_is_better,
        }
    }

    /// Return true if the change was an improvement.
    pub fn is_improvement(&self) -> bool {
        if self.lower_is_better {
            self.delta < 0.0
        } else {
            self.delta > 0.0
        }
    }

    /// Return a human-readable improvement percentage.
    pub fn improvement_pct(&self) -> f64 {
        if self.before == 0.0 {
            return 0.0;
        }
        (self.delta / self.before.abs()) * 100.0
    }
}

/// A single self-modification changelog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangelogEntry {
    /// Modification record ID.
    pub modification_id: String,
    /// Short summary of what changed.
    pub title: String,
    /// Explanation of *why* the change was made (problem being solved).
    pub rationale: String,
    /// Files that were changed.
    pub files_changed: Vec<String>,
    /// Metric deltas observed after deployment.
    pub metric_deltas: Vec<MetricDelta>,
    /// Who/what initiated this change.
    pub initiated_by: String,
    /// Unix timestamp.
    pub timestamp_secs: u64,
    /// Whether the change was ultimately kept (vs rolled back).
    pub kept: bool,
}

impl ChangelogEntry {
    /// Render this entry as a markdown section.
    pub fn to_markdown(&self) -> String {
        let ts = self.timestamp_secs;
        let kept_str = if self.kept {
            "✅ Deployed"
        } else {
            "🔄 Rolled back"
        };
        let mut md = format!(
            "## [{title}](#{id}) — {kept}\n\n\
             - **ID**: `{id}`\n\
             - **When**: Unix timestamp `{ts}`\n\
             - **By**: {by}\n\
             - **Status**: {kept}\n\n\
             ### Why\n\n{rationale}\n\n\
             ### Files changed\n\n",
            title = self.title,
            id = self.modification_id,
            kept = kept_str,
            ts = ts,
            by = self.initiated_by,
            rationale = self.rationale,
        );

        for f in &self.files_changed {
            md.push_str(&format!("- `{f}`\n"));
        }

        if !self.metric_deltas.is_empty() {
            md.push_str("\n### Performance impact\n\n");
            md.push_str("| Metric | Before | After | Delta | Result |\n");
            md.push_str("|--------|--------|-------|-------|--------|\n");
            for d in &self.metric_deltas {
                let result = if d.is_improvement() {
                    "✅ Improved"
                } else {
                    "⚠️ Degraded"
                };
                md.push_str(&format!(
                    "| {} | {:.3} | {:.3} | {:+.3} ({:+.1}%) | {} |\n",
                    d.name,
                    d.before,
                    d.after,
                    d.delta,
                    d.improvement_pct(),
                    result,
                ));
            }
        }

        md.push('\n');
        md
    }
}

/// A Mermaid diagram description for the current pipeline architecture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchitectureDiagram {
    /// Mermaid flowchart source.
    pub mermaid_source: String,
    /// Timestamp when generated.
    pub generated_at_secs: u64,
    /// Version or modification ID this diagram reflects.
    pub version: String,
}

impl ArchitectureDiagram {
    /// Render as a markdown code block.
    pub fn to_markdown(&self) -> String {
        format!(
            "```mermaid\n{}\n```\n\n_Generated at Unix `{}` (version `{}`)_\n",
            self.mermaid_source, self.generated_at_secs, self.version
        )
    }
}

/// A dependency impact analysis for a proposed change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyImpact {
    /// Files directly changed.
    pub changed_files: Vec<String>,
    /// Modules that transitively depend on the changed files.
    pub transitive_dependents: Vec<String>,
    /// Risk assessment: how many modules are affected.
    pub blast_radius: usize,
    /// Whether the change touches any public API surface.
    pub touches_public_api: bool,
}

impl DependencyImpact {
    /// Compute from a list of changed files and a simple module map.
    pub fn compute(
        changed_files: Vec<String>,
        dependency_map: &HashMap<String, Vec<String>>,
    ) -> Self {
        let mut transitive: Vec<String> = Vec::new();
        let mut visited = std::collections::HashSet::new();

        for file in &changed_files {
            collect_dependents(file, dependency_map, &mut transitive, &mut visited);
        }

        let blast_radius = transitive.len();
        let touches_public_api = changed_files
            .iter()
            .any(|f| f.contains("lib.rs") || f.contains("mod.rs") || f.contains("pub "));

        DependencyImpact {
            changed_files,
            transitive_dependents: transitive,
            blast_radius,
            touches_public_api,
        }
    }

    /// Render as markdown.
    pub fn to_markdown(&self) -> String {
        let api_warning = if self.touches_public_api {
            "⚠️ **This change touches public API surface — review carefully.**\n\n"
        } else {
            ""
        };

        let mut md = format!(
            "### Dependency impact\n\n{api_warning}\
             - **Blast radius**: {blast} modules affected\n\
             - **Changed files**: {count}\n\n",
            api_warning = api_warning,
            blast = self.blast_radius,
            count = self.changed_files.len(),
        );

        if !self.transitive_dependents.is_empty() {
            md.push_str("**Transitively affected modules:**\n\n");
            for dep in &self.transitive_dependents {
                md.push_str(&format!("- `{dep}`\n"));
            }
        }

        md
    }
}

fn collect_dependents(
    module: &str,
    map: &HashMap<String, Vec<String>>,
    result: &mut Vec<String>,
    visited: &mut std::collections::HashSet<String>,
) {
    for (parent, deps) in map {
        if deps.iter().any(|d| d == module) && !visited.contains(parent) {
            visited.insert(parent.clone());
            result.push(parent.clone());
            collect_dependents(parent, map, result, visited);
        }
    }
}

// ─── Documentation generator ─────────────────────────────────────────────────

struct DocsInner {
    changelog: Vec<ChangelogEntry>,
    diagrams: Vec<ArchitectureDiagram>,
    output_dir: String,
    max_changelog_entries: usize,
}

/// Generates and stores self-modification documentation.
#[derive(Clone)]
pub struct SelfDocGenerator {
    inner: Arc<Mutex<DocsInner>>,
}

impl SelfDocGenerator {
    /// Create a new generator.
    ///
    /// `output_dir` is where markdown files will be written.
    pub fn new(output_dir: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DocsInner {
                changelog: Vec::new(),
                diagrams: Vec::new(),
                output_dir: output_dir.into(),
                max_changelog_entries: 1000,
            })),
        }
    }

    /// Record a changelog entry.
    pub fn record_change(&self, entry: ChangelogEntry) -> Result<(), DocsError> {
        let mut inner = self.inner.lock().map_err(|_| DocsError::LockPoisoned)?;
        if inner.changelog.len() >= inner.max_changelog_entries {
            inner.changelog.remove(0);
        }
        inner.changelog.push(entry);
        Ok(())
    }

    /// Record an architecture diagram snapshot.
    pub fn record_diagram(&self, diagram: ArchitectureDiagram) -> Result<(), DocsError> {
        let mut inner = self.inner.lock().map_err(|_| DocsError::LockPoisoned)?;
        inner.diagrams.push(diagram);
        Ok(())
    }

    /// Return the full changelog (newest first).
    pub fn changelog(&self) -> Vec<ChangelogEntry> {
        self.inner
            .lock()
            .map(|inner| {
                let mut v = inner.changelog.clone();
                v.reverse();
                v
            })
            .unwrap_or_default()
    }

    /// Render the full changelog as a markdown document.
    pub fn render_changelog_md(&self) -> String {
        let entries = self.changelog();
        let mut md = "# Self-Modification Changelog\n\n".to_string();
        md.push_str(&format!("_Last updated: Unix `{}`_\n\n---\n\n", unix_now()));

        if entries.is_empty() {
            md.push_str("_No modifications recorded yet._\n");
        } else {
            for entry in &entries {
                md.push_str(&entry.to_markdown());
                md.push_str("---\n\n");
            }
        }
        md
    }

    /// Render the latest architecture diagram as markdown.
    pub fn render_latest_diagram_md(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.diagrams.last().cloned())
            .map(|d| d.to_markdown())
    }

    /// Generate the standard pipeline architecture Mermaid diagram.
    pub fn generate_pipeline_diagram(&self, version: &str) -> ArchitectureDiagram {
        let source = "flowchart LR\n\
            A[PromptRequest] --> B[RAG 512]\n\
            B --> C[Assemble 512]\n\
            C --> D[Inference 1024]\n\
            D --> E[PostProcess 512]\n\
            E --> F[Stream 256]\n\
            D -.-> G[(Circuit Breaker)]\n\
            D -.-> H[(Retry Policy)]\n\
            B -.-> I[(Dedup Cache)]\n\
            subgraph self-tune\n\
              J[Telemetry Bus]\n\
              K[PID Controller]\n\
              L[Anomaly Detector]\n\
            end\n\
            F -.-> J\n\
            J --> K\n\
            J --> L"
            .to_string();

        ArchitectureDiagram {
            mermaid_source: source,
            generated_at_secs: unix_now(),
            version: version.to_string(),
        }
    }

    /// Return the output directory for documentation files.
    pub fn output_dir(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.output_dir.clone())
            .unwrap_or_default()
    }

    /// Return the number of recorded changelog entries.
    pub fn entry_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.changelog.len())
            .unwrap_or(0)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str) -> ChangelogEntry {
        ChangelogEntry {
            modification_id: id.to_string(),
            title: format!("Fix {id}"),
            rationale: "Latency was too high".to_string(),
            files_changed: vec!["src/dedup.rs".to_string()],
            metric_deltas: vec![MetricDelta::new("p95_latency_ms", 10.0, 7.0, true)],
            initiated_by: "controller".to_string(),
            timestamp_secs: 1_700_000_000,
            kept: true,
        }
    }

    #[test]
    fn test_metric_delta_improvement_lower_is_better() {
        let d = MetricDelta::new("latency", 10.0, 7.0, true);
        assert!(d.is_improvement());
        assert!(d.delta < 0.0);
    }

    #[test]
    fn test_metric_delta_degradation_lower_is_better() {
        let d = MetricDelta::new("latency", 7.0, 10.0, true);
        assert!(!d.is_improvement());
    }

    #[test]
    fn test_metric_delta_improvement_higher_is_better() {
        let d = MetricDelta::new("throughput", 100.0, 120.0, false);
        assert!(d.is_improvement());
    }

    #[test]
    fn test_metric_delta_improvement_pct() {
        let d = MetricDelta::new("latency", 100.0, 80.0, true);
        assert!((d.improvement_pct() - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_metric_delta_zero_before() {
        let d = MetricDelta::new("m", 0.0, 5.0, false);
        assert_eq!(d.improvement_pct(), 0.0);
    }

    #[test]
    fn test_changelog_entry_to_markdown_contains_title() {
        let e = make_entry("mod-1");
        let md = e.to_markdown();
        assert!(md.contains("Fix mod-1"));
    }

    #[test]
    fn test_changelog_entry_to_markdown_shows_metrics() {
        let e = make_entry("mod-1");
        let md = e.to_markdown();
        assert!(md.contains("p95_latency_ms"));
    }

    #[test]
    fn test_architecture_diagram_to_markdown() {
        let diag = ArchitectureDiagram {
            mermaid_source: "flowchart LR\n  A --> B".to_string(),
            generated_at_secs: 0,
            version: "v1".to_string(),
        };
        let md = diag.to_markdown();
        assert!(md.contains("```mermaid"));
        assert!(md.contains("A --> B"));
    }

    #[test]
    fn test_dependency_impact_blast_radius() {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        map.insert("src/a.rs".into(), vec!["src/dedup.rs".into()]);
        map.insert("src/b.rs".into(), vec!["src/a.rs".into()]);
        let impact = DependencyImpact::compute(vec!["src/dedup.rs".into()], &map);
        assert!(impact.blast_radius >= 1);
    }

    #[test]
    fn test_dependency_impact_no_dependents() {
        let map: HashMap<String, Vec<String>> = HashMap::new();
        let impact = DependencyImpact::compute(vec!["src/isolated.rs".into()], &map);
        assert_eq!(impact.blast_radius, 0);
    }

    #[test]
    fn test_dependency_impact_to_markdown() {
        let map: HashMap<String, Vec<String>> = HashMap::new();
        let impact = DependencyImpact::compute(vec!["src/lib.rs".into()], &map);
        let md = impact.to_markdown();
        assert!(md.contains("Dependency impact"));
        assert!(md.contains("public API")); // touches_public_api should be true for lib.rs
    }

    #[test]
    fn test_generator_record_and_retrieve_changelog() {
        let gen = SelfDocGenerator::new("docs/self_modifications");
        gen.record_change(make_entry("m1")).unwrap();
        assert_eq!(gen.entry_count(), 1);
    }

    #[test]
    fn test_changelog_newest_first() {
        let gen = SelfDocGenerator::new("docs");
        gen.record_change(make_entry("a")).unwrap();
        gen.record_change(make_entry("b")).unwrap();
        let log = gen.changelog();
        assert_eq!(log[0].modification_id, "b");
    }

    #[test]
    fn test_render_changelog_md_empty() {
        let gen = SelfDocGenerator::new("docs");
        let md = gen.render_changelog_md();
        assert!(md.contains("No modifications recorded yet"));
    }

    #[test]
    fn test_render_changelog_md_with_entry() {
        let gen = SelfDocGenerator::new("docs");
        gen.record_change(make_entry("m1")).unwrap();
        let md = gen.render_changelog_md();
        assert!(md.contains("Fix m1"));
    }

    #[test]
    fn test_generate_pipeline_diagram() {
        let gen = SelfDocGenerator::new("docs");
        let diag = gen.generate_pipeline_diagram("v1.2.3");
        assert!(diag.mermaid_source.contains("flowchart"));
        assert_eq!(diag.version, "v1.2.3");
    }

    #[test]
    fn test_record_and_render_latest_diagram() {
        let gen = SelfDocGenerator::new("docs");
        let diag = gen.generate_pipeline_diagram("v1");
        gen.record_diagram(diag).unwrap();
        let md = gen.render_latest_diagram_md();
        assert!(md.is_some());
        assert!(md.unwrap().contains("mermaid"));
    }

    #[test]
    fn test_output_dir_returned() {
        let gen = SelfDocGenerator::new("/tmp/docs");
        assert_eq!(gen.output_dir(), "/tmp/docs");
    }

    #[test]
    fn test_generator_clone_shares_state() {
        let gen = SelfDocGenerator::new("docs");
        let gen2 = gen.clone();
        gen.record_change(make_entry("x")).unwrap();
        assert_eq!(gen2.entry_count(), 1);
    }

    #[test]
    fn test_kept_false_shows_rolled_back() {
        let mut entry = make_entry("m2");
        entry.kept = false;
        let md = entry.to_markdown();
        assert!(md.contains("Rolled back"));
    }

    #[test]
    fn test_max_entries_cap() {
        let gen = SelfDocGenerator::new("docs");
        {
            let mut inner = gen.inner.lock().unwrap();
            inner.max_changelog_entries = 3;
        }
        for i in 0..5 {
            gen.record_change(make_entry(&format!("m{i}"))).unwrap();
        }
        assert_eq!(gen.entry_count(), 3);
    }
}
