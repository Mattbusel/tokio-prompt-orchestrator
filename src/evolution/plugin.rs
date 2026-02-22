//! # Stage: PluginManager (Task 4.3 -- Plugin Architecture)
//!
//! ## Responsibility
//! Extensible plugin system for adding custom behaviours to the orchestrator.
//! Plugins register manifests declaring which extension points (hooks) they
//! participate in. The manager maintains a registry and execution log.
//!
//! ## Guarantees
//! - Thread-safe: all operations use `Arc<Mutex<Inner>>`
//! - Bounded: execution log is capped at `max_log` entries
//! - Non-panicking: every public method returns `Result`
//! - Idempotent: enable/disable are safe to call repeatedly
//!
//! ## NOT Responsible For
//! - Plugin discovery or download (manifests are provided externally)
//! - Sandboxing or security isolation
//! - Actual hook execution (this is a registry/manifest system)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors specific to the plugin system.
#[derive(Debug, Error)]
pub enum PluginError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("plugin manager lock poisoned")]
    LockPoisoned,

    /// The requested plugin was not found in the registry.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),

    /// A plugin with the same ID is already registered.
    #[error("plugin already registered: {0}")]
    PluginAlreadyRegistered(String),

    /// The plugin manifest failed validation.
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    /// The requested hook point was not found.
    #[error("hook not found: {0}")]
    HookNotFound(String),
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Extension points where plugins can attach behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPoint {
    /// Before inference is executed.
    PreInference,
    /// After inference completes.
    PostInference,
    /// Before routing decisions are made.
    PreRouting,
    /// After routing decisions are made.
    PostRouting,
    /// When an error occurs in the pipeline.
    OnError,
    /// When a metric is updated.
    OnMetricUpdate,
    /// When configuration changes.
    OnConfigChange,
    /// A user-defined custom hook point.
    Custom(String),
}

/// Manifest describing a plugin and its capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Unique identifier for the plugin.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Semantic version string.
    pub version: String,
    /// Short description of the plugin's purpose.
    pub description: String,
    /// Author or organisation name.
    pub author: String,
    /// Hook points this plugin registers for.
    pub hooks: Vec<HookPoint>,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
}

/// Record of a single hook execution for auditing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookExecution {
    /// ID of the plugin that ran.
    pub plugin_id: String,
    /// The hook point that was executed.
    pub hook: HookPoint,
    /// Unix timestamp (seconds) when execution occurred.
    pub timestamp_secs: u64,
    /// Whether execution succeeded.
    pub success: bool,
    /// Human-readable message or error description.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Inner {
    manifests: HashMap<String, PluginManifest>,
    hook_registry: HashMap<HookPoint, Vec<String>>,
    execution_log: Vec<HookExecution>,
    max_log: usize,
}

// ---------------------------------------------------------------------------
// PluginManager
// ---------------------------------------------------------------------------

/// Registry and lifecycle manager for plugins.
///
/// Cheap to clone -- all clones share the same inner state via `Arc<Mutex<_>>`.
#[derive(Debug, Clone)]
pub struct PluginManager {
    inner: Arc<Mutex<Inner>>,
}

impl PluginManager {
    /// Create a new empty `PluginManager`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                manifests: HashMap::new(),
                hook_registry: HashMap::new(),
                execution_log: Vec::new(),
                max_log: 10_000,
            })),
        }
    }

    /// Register a plugin by its manifest.
    ///
    /// # Errors
    /// - [`PluginError::PluginAlreadyRegistered`] if the ID already exists.
    /// - [`PluginError::InvalidManifest`] if the manifest is missing required fields.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn register(&self, manifest: PluginManifest) -> Result<(), PluginError> {
        if manifest.id.is_empty() {
            return Err(PluginError::InvalidManifest(
                "plugin id must not be empty".to_string(),
            ));
        }
        if manifest.name.is_empty() {
            return Err(PluginError::InvalidManifest(
                "plugin name must not be empty".to_string(),
            ));
        }

        let mut inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;

        if inner.manifests.contains_key(&manifest.id) {
            return Err(PluginError::PluginAlreadyRegistered(manifest.id.clone()));
        }

        // Register hooks
        for hook in &manifest.hooks {
            inner
                .hook_registry
                .entry(hook.clone())
                .or_default()
                .push(manifest.id.clone());
        }

        inner.manifests.insert(manifest.id.clone(), manifest);
        Ok(())
    }

    /// Remove a plugin from the registry.
    ///
    /// # Errors
    /// - [`PluginError::PluginNotFound`] if the ID is unknown.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn unregister(&self, plugin_id: &str) -> Result<(), PluginError> {
        let mut inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;

        if inner.manifests.remove(plugin_id).is_none() {
            return Err(PluginError::PluginNotFound(plugin_id.to_string()));
        }

        // Remove from hook registry
        for plugins in inner.hook_registry.values_mut() {
            plugins.retain(|id| id != plugin_id);
        }

        Ok(())
    }

    /// Enable a registered plugin.
    ///
    /// # Errors
    /// - [`PluginError::PluginNotFound`] if the ID is unknown.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn enable(&self, plugin_id: &str) -> Result<(), PluginError> {
        let mut inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;
        let manifest = inner
            .manifests
            .get_mut(plugin_id)
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))?;
        manifest.enabled = true;
        Ok(())
    }

    /// Disable a registered plugin.
    ///
    /// # Errors
    /// - [`PluginError::PluginNotFound`] if the ID is unknown.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn disable(&self, plugin_id: &str) -> Result<(), PluginError> {
        let mut inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;
        let manifest = inner
            .manifests
            .get_mut(plugin_id)
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))?;
        manifest.enabled = false;
        Ok(())
    }

    /// Check whether a plugin is currently enabled.
    ///
    /// # Errors
    /// - [`PluginError::PluginNotFound`] if the ID is unknown.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn is_enabled(&self, plugin_id: &str) -> Result<bool, PluginError> {
        let inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;
        let manifest = inner
            .manifests
            .get(plugin_id)
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))?;
        Ok(manifest.enabled)
    }

    /// Retrieve a copy of a plugin's manifest.
    ///
    /// # Errors
    /// - [`PluginError::PluginNotFound`] if the ID is unknown.
    /// - [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get_manifest(&self, plugin_id: &str) -> Result<PluginManifest, PluginError> {
        let inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;
        inner
            .manifests
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))
    }

    /// Return IDs of all plugins registered for the given hook point.
    ///
    /// # Panics
    /// This function never panics.
    pub fn plugins_for_hook(&self, hook: &HookPoint) -> Vec<String> {
        match self.inner.lock() {
            Ok(inner) => inner.hook_registry.get(hook).cloned().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Append an execution record to the audit log.
    ///
    /// # Errors
    /// Returns [`PluginError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn log_execution(&self, execution: HookExecution) -> Result<(), PluginError> {
        let mut inner = self.inner.lock().map_err(|_| PluginError::LockPoisoned)?;
        if inner.execution_log.len() >= inner.max_log {
            inner.execution_log.remove(0);
        }
        inner.execution_log.push(execution);
        Ok(())
    }

    /// Return a snapshot of the execution log.
    ///
    /// # Panics
    /// This function never panics.
    pub fn execution_log(&self) -> Vec<HookExecution> {
        match self.inner.lock() {
            Ok(inner) => inner.execution_log.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Return the number of registered plugins.
    ///
    /// # Panics
    /// This function never panics.
    pub fn plugin_count(&self) -> usize {
        match self.inner.lock() {
            Ok(inner) => inner.manifests.len(),
            Err(_) => 0,
        }
    }

    /// Return manifests for all registered plugins.
    ///
    /// # Panics
    /// This function never panics.
    pub fn all_manifests(&self) -> Vec<PluginManifest> {
        match self.inner.lock() {
            Ok(inner) => inner.manifests.values().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(id: &str) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            name: format!("Test Plugin {id}"),
            version: "1.0.0".to_string(),
            description: "A test plugin".to_string(),
            author: "test".to_string(),
            hooks: vec![HookPoint::PreInference, HookPoint::PostInference],
            enabled: true,
        }
    }

    #[test]
    fn test_register_plugin() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        assert_eq!(mgr.plugin_count(), 1);
    }

    #[test]
    fn test_unregister_plugin() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        mgr.unregister("plug-1").unwrap();
        assert_eq!(mgr.plugin_count(), 0);
    }

    #[test]
    fn test_duplicate_registration_error() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        let err = mgr.register(sample_manifest("plug-1")).unwrap_err();
        assert!(matches!(err, PluginError::PluginAlreadyRegistered(_)));
    }

    #[test]
    fn test_plugin_not_found_error() {
        let mgr = PluginManager::new();
        let err = mgr.unregister("nonexistent").unwrap_err();
        assert!(matches!(err, PluginError::PluginNotFound(_)));
    }

    #[test]
    fn test_enable_disable() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        assert!(mgr.is_enabled("plug-1").unwrap());
        mgr.disable("plug-1").unwrap();
        assert!(!mgr.is_enabled("plug-1").unwrap());
        mgr.enable("plug-1").unwrap();
        assert!(mgr.is_enabled("plug-1").unwrap());
    }

    #[test]
    fn test_enable_not_found() {
        let mgr = PluginManager::new();
        let err = mgr.enable("nope").unwrap_err();
        assert!(matches!(err, PluginError::PluginNotFound(_)));
    }

    #[test]
    fn test_disable_not_found() {
        let mgr = PluginManager::new();
        let err = mgr.disable("nope").unwrap_err();
        assert!(matches!(err, PluginError::PluginNotFound(_)));
    }

    #[test]
    fn test_plugins_for_hook() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        let plugins = mgr.plugins_for_hook(&HookPoint::PreInference);
        assert_eq!(plugins, vec!["plug-1".to_string()]);
    }

    #[test]
    fn test_plugins_for_hook_empty() {
        let mgr = PluginManager::new();
        let plugins = mgr.plugins_for_hook(&HookPoint::OnError);
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_unregister_removes_from_hooks() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        mgr.unregister("plug-1").unwrap();
        let plugins = mgr.plugins_for_hook(&HookPoint::PreInference);
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_log_execution() {
        let mgr = PluginManager::new();
        let exec = HookExecution {
            plugin_id: "plug-1".to_string(),
            hook: HookPoint::PreInference,
            timestamp_secs: 1000,
            success: true,
            message: "ok".to_string(),
        };
        mgr.log_execution(exec).unwrap();
        let log = mgr.execution_log();
        assert_eq!(log.len(), 1);
        assert!(log[0].success);
    }

    #[test]
    fn test_all_manifests() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        mgr.register(sample_manifest("plug-2")).unwrap();
        let manifests = mgr.all_manifests();
        assert_eq!(manifests.len(), 2);
    }

    #[test]
    fn test_get_manifest() {
        let mgr = PluginManager::new();
        mgr.register(sample_manifest("plug-1")).unwrap();
        let m = mgr.get_manifest("plug-1").unwrap();
        assert_eq!(m.id, "plug-1");
        assert_eq!(m.version, "1.0.0");
    }

    #[test]
    fn test_get_manifest_not_found() {
        let mgr = PluginManager::new();
        let err = mgr.get_manifest("nope").unwrap_err();
        assert!(matches!(err, PluginError::PluginNotFound(_)));
    }

    #[test]
    fn test_invalid_manifest_empty_id() {
        let mgr = PluginManager::new();
        let mut manifest = sample_manifest("test");
        manifest.id = String::new();
        let err = mgr.register(manifest).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn test_invalid_manifest_empty_name() {
        let mgr = PluginManager::new();
        let mut manifest = sample_manifest("test");
        manifest.name = String::new();
        let err = mgr.register(manifest).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn test_clone_shares_state() {
        let mgr = PluginManager::new();
        let mgr2 = mgr.clone();
        mgr.register(sample_manifest("plug-1")).unwrap();
        assert_eq!(mgr2.plugin_count(), 1);
    }

    #[test]
    fn test_serialization_manifest() {
        let manifest = sample_manifest("plug-1");
        let json = serde_json::to_string(&manifest).unwrap();
        let back: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "plug-1");
        assert_eq!(back.hooks.len(), 2);
    }

    #[test]
    fn test_serialization_hook_point() {
        let hooks = vec![
            HookPoint::PreInference,
            HookPoint::PostInference,
            HookPoint::OnError,
            HookPoint::Custom("my-hook".to_string()),
        ];
        for hook in &hooks {
            let json = serde_json::to_string(hook).unwrap();
            let back: HookPoint = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, hook);
        }
    }

    #[test]
    fn test_serialization_hook_execution() {
        let exec = HookExecution {
            plugin_id: "p1".to_string(),
            hook: HookPoint::OnMetricUpdate,
            timestamp_secs: 12345,
            success: false,
            message: "timeout".to_string(),
        };
        let json = serde_json::to_string(&exec).unwrap();
        let back: HookExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plugin_id, "p1");
        assert!(!back.success);
    }

    #[test]
    fn test_custom_hook_point() {
        let mgr = PluginManager::new();
        let mut manifest = sample_manifest("custom-plug");
        manifest.hooks = vec![HookPoint::Custom("my-custom-hook".to_string())];
        mgr.register(manifest).unwrap();
        let plugins = mgr.plugins_for_hook(&HookPoint::Custom("my-custom-hook".to_string()));
        assert_eq!(plugins, vec!["custom-plug".to_string()]);
    }

    #[test]
    fn test_error_display() {
        let err = PluginError::PluginNotFound("x".to_string());
        assert!(err.to_string().contains("x"));

        let err = PluginError::PluginAlreadyRegistered("y".to_string());
        assert!(err.to_string().contains("y"));

        let err = PluginError::InvalidManifest("bad".to_string());
        assert!(err.to_string().contains("bad"));
    }

    #[test]
    fn test_default_impl() {
        let mgr = PluginManager::default();
        assert_eq!(mgr.plugin_count(), 0);
    }
}
