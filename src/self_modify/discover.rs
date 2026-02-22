//! # Capability Discovery (Task 2.6)
//!
//! Periodic analysis of the codebase for improvement opportunities
//! beyond anomaly-driven fixes:
//!
//! - Dead code detection and removal proposals
//! - Dependency audit (outdated crates, security advisories)
//! - Test coverage gaps (untested code paths)
//! - Performance profiling hotspot analysis
//! - API surface analysis (unused / missing MCP tools)
//!
//! Generates tasks for the coordinator.
//! Lower priority than anomaly-driven tasks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Error ────────────────────────────────────────────────────────────────────

/// Errors from the capability discovery module.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// Lock poisoned.
    #[error("discovery lock poisoned")]
    LockPoisoned,

    /// The workspace path could not be accessed.
    #[error("workspace error: {0}")]
    WorkspaceError(String),
}

// ─── Discovery finding ────────────────────────────────────────────────────────

/// Category of a discovery finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingCategory {
    /// Unused code that can be safely removed.
    DeadCode,
    /// An outdated or vulnerable dependency.
    Dependency,
    /// Missing or insufficient test coverage.
    TestCoverage,
    /// Performance hotspot identified through profiling.
    Performance,
    /// API surface issue (unused tools, missing tools).
    ApiSurface,
}

impl FindingCategory {
    /// Return a short human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::DeadCode => "dead-code",
            Self::Dependency => "dependency",
            Self::TestCoverage => "test-coverage",
            Self::Performance => "performance",
            Self::ApiSurface => "api-surface",
        }
    }
}

/// A single discovery finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryFinding {
    /// Unique finding ID.
    pub id: String,
    /// Category of the finding.
    pub category: FindingCategory,
    /// Short description of the issue.
    pub title: String,
    /// Detailed description with context.
    pub description: String,
    /// Files affected.
    pub affected_files: Vec<String>,
    /// Suggested next action.
    pub suggested_action: String,
    /// Estimated effort to fix (lines of code changed, or "low/medium/high").
    pub estimated_effort: String,
    /// Unix timestamp when discovered.
    pub discovered_at_secs: u64,
    /// Whether this finding has been addressed.
    pub resolved: bool,
}

impl DiscoveryFinding {
    /// Render as a markdown section.
    pub fn to_markdown(&self) -> String {
        format!(
            "### [{cat}] {title}\n\n\
             **ID**: `{id}`  \n\
             **Effort**: {effort}  \n\
             **Status**: {status}\n\n\
             {desc}\n\n\
             **Suggested action**: {action}\n\n\
             **Affected files**:\n{files}\n",
            cat = self.category.label(),
            title = self.title,
            id = self.id,
            effort = self.estimated_effort,
            status = if self.resolved {
                "✅ Resolved"
            } else {
                "🔍 Open"
            },
            desc = self.description,
            action = self.suggested_action,
            files = self
                .affected_files
                .iter()
                .map(|f| format!("- `{f}`"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

// ─── Scan result ─────────────────────────────────────────────────────────────

/// Result of a single discovery scan run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    /// All findings from this scan.
    pub findings: Vec<DiscoveryFinding>,
    /// Duration of the scan.
    pub scan_duration_ms: u64,
    /// Unix timestamp when the scan completed.
    pub scanned_at_secs: u64,
    /// Which categories were scanned.
    pub categories_scanned: Vec<FindingCategory>,
}

// ─── Scanner configuration ────────────────────────────────────────────────────

/// Configuration for the capability discovery scanner.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Path to the workspace root.
    pub workspace_path: String,
    /// Categories to enable.
    pub enabled_categories: Vec<FindingCategory>,
    /// Minimum interval between scheduled scans.
    pub scan_interval: Duration,
    /// Ignore paths matching these prefixes.
    pub ignore_paths: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            workspace_path: ".".to_string(),
            enabled_categories: vec![
                FindingCategory::DeadCode,
                FindingCategory::Dependency,
                FindingCategory::TestCoverage,
                FindingCategory::Performance,
                FindingCategory::ApiSurface,
            ],
            scan_interval: Duration::from_secs(3600),
            ignore_paths: vec!["target/".to_string(), ".git/".to_string()],
        }
    }
}

// ─── Scanner ─────────────────────────────────────────────────────────────────

struct DiscoveryInner {
    cfg: DiscoveryConfig,
    findings: Vec<DiscoveryFinding>,
    scan_history: Vec<ScanResult>,
    last_scan_at: Option<u64>,
    finding_counter: u64,
}

/// Periodic codebase capability scanner.
#[derive(Clone)]
pub struct CapabilityDiscovery {
    inner: Arc<Mutex<DiscoveryInner>>,
}

impl CapabilityDiscovery {
    /// Create a new scanner with the given config.
    pub fn new(cfg: DiscoveryConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DiscoveryInner {
                cfg,
                findings: Vec::new(),
                scan_history: Vec::new(),
                last_scan_at: None,
                finding_counter: 0,
            })),
        }
    }

    /// Run a synchronous scan over the configured workspace.
    ///
    /// In production this would invoke `cargo check`, `cargo audit`,
    /// and coverage tools.  This implementation provides a structured
    /// stub that can be extended.
    pub fn scan(&self) -> Result<ScanResult, DiscoveryError> {
        let start = std::time::Instant::now();
        let now = unix_now();

        let (workspace, categories, ignore) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| DiscoveryError::LockPoisoned)?;
            (
                inner.cfg.workspace_path.clone(),
                inner.cfg.enabled_categories.clone(),
                inner.cfg.ignore_paths.clone(),
            )
        };

        let mut new_findings: Vec<DiscoveryFinding> = Vec::new();

        for category in &categories {
            let mut cat_findings = self.scan_category(*category, &workspace, &ignore)?;
            new_findings.append(&mut cat_findings);
        }

        let elapsed = start.elapsed().as_millis() as u64;

        let result = ScanResult {
            findings: new_findings.clone(),
            scan_duration_ms: elapsed,
            scanned_at_secs: now,
            categories_scanned: categories,
        };

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DiscoveryError::LockPoisoned)?;
        for f in new_findings {
            inner.findings.push(f);
        }
        if inner.scan_history.len() >= 100 {
            inner.scan_history.remove(0);
        }
        inner.scan_history.push(result.clone());
        inner.last_scan_at = Some(now);

        Ok(result)
    }

    /// Return all open (unresolved) findings.
    pub fn open_findings(&self) -> Vec<DiscoveryFinding> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .findings
                    .iter()
                    .filter(|f| !f.resolved)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return all findings of a given category.
    pub fn findings_by_category(&self, category: FindingCategory) -> Vec<DiscoveryFinding> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .findings
                    .iter()
                    .filter(|f| f.category == category)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Mark a finding as resolved.
    pub fn resolve(&self, finding_id: &str) -> Result<bool, DiscoveryError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DiscoveryError::LockPoisoned)?;
        if let Some(f) = inner.findings.iter_mut().find(|f| f.id == finding_id) {
            f.resolved = true;
            return Ok(true);
        }
        Ok(false)
    }

    /// Manually register a finding (e.g., from an external tool's output).
    pub fn register_finding(
        &self,
        category: FindingCategory,
        title: impl Into<String>,
        description: impl Into<String>,
        affected_files: Vec<String>,
        suggested_action: impl Into<String>,
        effort: impl Into<String>,
    ) -> Result<String, DiscoveryError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DiscoveryError::LockPoisoned)?;
        inner.finding_counter += 1;
        let id = format!("finding-{:06}", inner.finding_counter);
        inner.findings.push(DiscoveryFinding {
            id: id.clone(),
            category,
            title: title.into(),
            description: description.into(),
            affected_files,
            suggested_action: suggested_action.into(),
            estimated_effort: effort.into(),
            discovered_at_secs: unix_now(),
            resolved: false,
        });
        Ok(id)
    }

    /// Return a summary of finding counts by category.
    pub fn summary(&self) -> HashMap<String, usize> {
        let findings = self.open_findings();
        let mut map: HashMap<String, usize> = HashMap::new();
        for f in findings {
            *map.entry(f.category.label().to_string()).or_insert(0) += 1;
        }
        map
    }

    /// Return the last scan result, if any.
    pub fn last_scan(&self) -> Option<ScanResult> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.scan_history.last().cloned())
    }

    /// Return Unix timestamp of the last scan, if any.
    pub fn last_scan_at(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|inner| inner.last_scan_at)
    }

    /// Return the total number of findings (open + resolved).
    pub fn total_finding_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.findings.len())
            .unwrap_or(0)
    }

    // ── Internal scan implementations ──────────────────────────────────────

    fn scan_category(
        &self,
        category: FindingCategory,
        _workspace: &str,
        _ignore: &[String],
    ) -> Result<Vec<DiscoveryFinding>, DiscoveryError> {
        // Each category has a stub implementation.
        // In production these call out to cargo check, cargo audit, llvm-cov, etc.
        match category {
            FindingCategory::DeadCode => Ok(self.scan_dead_code()),
            FindingCategory::Dependency => Ok(self.scan_dependencies()),
            FindingCategory::TestCoverage => Ok(self.scan_test_coverage()),
            FindingCategory::Performance => Ok(self.scan_performance()),
            FindingCategory::ApiSurface => Ok(self.scan_api_surface()),
        }
    }

    fn scan_dead_code(&self) -> Vec<DiscoveryFinding> {
        // Run `cargo check --message-format=json` and parse compiler warnings
        // for dead_code, unused_imports, unused_variables, and similar lints.
        let workspace = {
            match self.inner.lock() {
                Ok(g) => g.cfg.workspace_path.clone(),
                Err(_) => return Vec::new(),
            }
        };

        let output = match std::process::Command::new("cargo")
            .args(["check", "--message-format=json", "--quiet"])
            .current_dir(&workspace)
            .output()
        {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };

        let dead_code_codes: &[&str] = &[
            "dead_code",
            "unused_imports",
            "unused_variables",
            "unused_mut",
            "unused_assignments",
        ];

        let mut findings = Vec::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
                continue;
            }
            let message = &msg["message"];
            let level = message
                .get("level")
                .and_then(|l| l.as_str())
                .unwrap_or("note");
            if level != "warning" {
                continue;
            }
            let code = message
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if !dead_code_codes.contains(&code) {
                continue;
            }
            let rendered = message
                .get("rendered")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            let file_path = message
                .get("spans")
                .and_then(|s| s.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.get("file_name"))
                .and_then(|f| f.as_str())
                .unwrap_or("unknown")
                .to_string();

            findings.push(DiscoveryFinding {
                id: format!("dead-{}-{}", code, findings.len()),
                category: FindingCategory::DeadCode,
                title: format!("dead_code lint: {code}"),
                description: if rendered.is_empty() {
                    format!("cargo check warning: {code}")
                } else {
                    rendered.chars().take(500).collect()
                },
                affected_files: vec![file_path],
                suggested_action: "review and remove unused code".into(),
                estimated_effort: "low".into(),
                discovered_at_secs: unix_now(),
                resolved: false,
            });
        }
        findings
    }

    fn scan_dependencies(&self) -> Vec<DiscoveryFinding> {
        // Run `cargo audit --json` (if available) and report vulnerability advisories.
        let workspace = {
            match self.inner.lock() {
                Ok(g) => g.cfg.workspace_path.clone(),
                Err(_) => return Vec::new(),
            }
        };

        let output = match std::process::Command::new("cargo")
            .args(["audit", "--json"])
            .current_dir(&workspace)
            .output()
        {
            Ok(o) => o,
            Err(_) => {
                // cargo-audit not installed — return empty, don't fail scan
                return Vec::new();
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let Ok(report) = serde_json::from_str::<serde_json::Value>(&stdout) else {
            return Vec::new();
        };

        let mut findings = Vec::new();
        if let Some(vulns) = report
            .get("vulnerabilities")
            .and_then(|v| v.get("list"))
            .and_then(|l| l.as_array())
        {
            for vuln in vulns {
                let id = vuln
                    .get("advisory")
                    .and_then(|a| a.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or("RUSTSEC-UNKNOWN")
                    .to_string();
                let title = vuln
                    .get("advisory")
                    .and_then(|a| a.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown vulnerability")
                    .to_string();
                let package = vuln
                    .get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                findings.push(DiscoveryFinding {
                    id: format!("dep-{id}"),
                    category: FindingCategory::Dependency,
                    title: format!("{id}: {title}"),
                    description: format!("Vulnerable dependency: {package}. Advisory: {id}"),
                    affected_files: vec!["Cargo.lock".into(), "Cargo.toml".into()],
                    suggested_action: format!("upgrade or replace {package} to resolve {id}"),
                    estimated_effort: "medium".into(),
                    discovered_at_secs: unix_now(),
                    resolved: false,
                });
            }
        }
        findings
    }

    fn scan_test_coverage(&self) -> Vec<DiscoveryFinding> {
        // Coverage requires llvm-cov; stub returns empty until tooling is available.
        // The dead_code scan is a partial proxy for missing tests.
        Vec::new()
    }

    fn scan_performance(&self) -> Vec<DiscoveryFinding> {
        // Performance hotspot analysis requires criterion benchmark output.
        // Stub returns empty — performance findings come from telemetry-driven
        // anomaly detection instead.
        Vec::new()
    }

    fn scan_api_surface(&self) -> Vec<DiscoveryFinding> {
        // API surface analysis requires cross-referencing MCP tool registrations
        // against usage logs. Stub returns empty pending MCP integration.
        Vec::new()
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

    fn make_scanner() -> CapabilityDiscovery {
        CapabilityDiscovery::new(DiscoveryConfig::default())
    }

    #[test]
    fn test_scan_completes_without_error() {
        let s = make_scanner();
        assert!(s.scan().is_ok());
    }

    #[test]
    fn test_scan_stores_result() {
        let s = make_scanner();
        s.scan().unwrap();
        assert!(s.last_scan().is_some());
    }

    #[test]
    fn test_last_scan_at_set_after_scan() {
        let s = make_scanner();
        s.scan().unwrap();
        assert!(s.last_scan_at().is_some());
    }

    #[test]
    fn test_register_finding_returns_id() {
        let s = make_scanner();
        let id = s
            .register_finding(
                FindingCategory::DeadCode,
                "Unused fn foo",
                "fn foo is never called",
                vec!["src/foo.rs".into()],
                "Remove the function",
                "low",
            )
            .unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn test_open_findings_contains_registered() {
        let s = make_scanner();
        s.register_finding(FindingCategory::DeadCode, "t", "d", vec![], "a", "low")
            .unwrap();
        assert_eq!(s.open_findings().len(), 1);
    }

    #[test]
    fn test_resolve_finding_removes_from_open() {
        let s = make_scanner();
        let id = s
            .register_finding(FindingCategory::DeadCode, "t", "d", vec![], "a", "low")
            .unwrap();
        s.resolve(&id).unwrap();
        assert!(s.open_findings().is_empty());
    }

    #[test]
    fn test_resolve_nonexistent_returns_false() {
        let s = make_scanner();
        let result = s.resolve("does-not-exist").unwrap();
        assert!(!result);
    }

    #[test]
    fn test_findings_by_category_filters() {
        let s = make_scanner();
        s.register_finding(FindingCategory::DeadCode, "d1", "x", vec![], "a", "low")
            .unwrap();
        s.register_finding(FindingCategory::Dependency, "dep1", "y", vec![], "a", "low")
            .unwrap();
        let dead = s.findings_by_category(FindingCategory::DeadCode);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].category, FindingCategory::DeadCode);
    }

    #[test]
    fn test_summary_counts_by_category() {
        let s = make_scanner();
        s.register_finding(FindingCategory::DeadCode, "a", "x", vec![], "act", "low")
            .unwrap();
        s.register_finding(FindingCategory::DeadCode, "b", "x", vec![], "act", "low")
            .unwrap();
        s.register_finding(FindingCategory::Dependency, "c", "x", vec![], "act", "low")
            .unwrap();
        let summary = s.summary();
        assert_eq!(*summary.get("dead-code").unwrap_or(&0), 2);
        assert_eq!(*summary.get("dependency").unwrap_or(&0), 1);
    }

    #[test]
    fn test_total_finding_count_includes_resolved() {
        let s = make_scanner();
        let id = s
            .register_finding(FindingCategory::DeadCode, "a", "d", vec![], "x", "low")
            .unwrap();
        s.resolve(&id).unwrap();
        assert_eq!(s.total_finding_count(), 1);
    }

    #[test]
    fn test_finding_to_markdown_contains_title() {
        let f = DiscoveryFinding {
            id: "f-1".into(),
            category: FindingCategory::Performance,
            title: "Hot loop in dedup".into(),
            description: "dedup check is called 100k/s".into(),
            affected_files: vec!["src/dedup.rs".into()],
            suggested_action: "Cache the bloom filter result".into(),
            estimated_effort: "medium".into(),
            discovered_at_secs: 0,
            resolved: false,
        };
        let md = f.to_markdown();
        assert!(md.contains("Hot loop in dedup"));
        assert!(md.contains("performance"));
    }

    #[test]
    fn test_finding_category_label() {
        assert_eq!(FindingCategory::DeadCode.label(), "dead-code");
        assert_eq!(FindingCategory::TestCoverage.label(), "test-coverage");
        assert_eq!(FindingCategory::ApiSurface.label(), "api-surface");
    }

    #[test]
    fn test_scanner_clone_shares_findings() {
        let s = make_scanner();
        let s2 = s.clone();
        s.register_finding(FindingCategory::DeadCode, "x", "y", vec![], "z", "low")
            .unwrap();
        assert_eq!(s2.open_findings().len(), 1);
    }

    #[test]
    fn test_finding_id_increments() {
        let s = make_scanner();
        let id1 = s
            .register_finding(FindingCategory::DeadCode, "a", "d", vec![], "x", "low")
            .unwrap();
        let id2 = s
            .register_finding(FindingCategory::DeadCode, "b", "d", vec![], "x", "low")
            .unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_config_default_includes_all_categories() {
        let cfg = DiscoveryConfig::default();
        assert_eq!(cfg.enabled_categories.len(), 5);
    }

    #[test]
    fn test_scan_result_fields_present() {
        let s = make_scanner();
        let result = s.scan().unwrap();
        assert!(result.scanned_at_secs > 0);
        assert!(!result.categories_scanned.is_empty());
    }

    #[test]
    fn test_scan_history_capped_at_100() {
        let s = make_scanner();
        for _ in 0..105 {
            s.scan().unwrap();
        }
        let inner = s.inner.lock().unwrap();
        assert!(inner.scan_history.len() <= 100);
    }

    #[test]
    fn test_resolved_finding_shows_in_markdown() {
        let mut f = DiscoveryFinding {
            id: "f-2".into(),
            category: FindingCategory::DeadCode,
            title: "Test".into(),
            description: String::new(),
            affected_files: vec![],
            suggested_action: String::new(),
            estimated_effort: String::new(),
            discovered_at_secs: 0,
            resolved: true,
        };
        let md = f.to_markdown();
        assert!(md.contains("Resolved"));
        f.resolved = false;
        let md2 = f.to_markdown();
        assert!(md2.contains("Open"));
    }
}
