//! # Stage: TransferLearning (Task 4.4 -- Cross-System Knowledge Transfer)
//!
//! ## Responsibility
//! Package learned configurations and performance insights for transfer between
//! orchestrator instances. Exports knowledge as portable packets that can be
//! imported by another system.
//!
//! ## Guarantees
//! - Thread-safe: all operations use `Arc<Mutex<Inner>>`
//! - Bounded: stored packets capped at `max_packets`
//! - Versioned: incompatible versions are rejected at import time
//! - Non-panicking: every public method returns `Result`
//!
//! ## NOT Responsible For
//! - Network transport (packets are passed in-process or serialised externally)
//! - Authentication or authorisation

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors specific to knowledge transfer.
#[derive(Debug, Error)]
pub enum TransferError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("transfer learning lock poisoned")]
    LockPoisoned,

    /// The knowledge packet failed validation.
    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    /// The packet version is not compatible with this system.
    #[error("incompatible version: expected '{expected}', got '{got}'")]
    IncompatibleVersion {
        /// The version this system expects.
        expected: String,
        /// The version found in the packet.
        got: String,
    },

    /// The requested packet was not found.
    #[error("packet not found: {0}")]
    PacketNotFound(String),

    /// The packet confidence is below the configured minimum.
    #[error("confidence too low: {confidence} < minimum {minimum}")]
    LowConfidence {
        /// The actual confidence value.
        confidence: f64,
        /// The minimum required confidence.
        minimum: f64,
    },
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Configuration for the transfer learning subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    /// Protocol version for packet compatibility.
    pub version: String,
    /// Minimum confidence required to accept a packet.
    pub min_confidence: f64,
    /// Maximum age (seconds) before a packet is considered stale.
    pub max_packet_age_secs: u64,
    /// Maximum number of packets to retain.
    pub max_packets: usize,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            version: "1.0".to_string(),
            min_confidence: 0.7,
            max_packet_age_secs: 86400,
            max_packets: 1000,
        }
    }
}

/// A portable bundle of learned knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgePacket {
    /// Unique identifier for this packet.
    pub id: String,
    /// Identifier of the system that created this packet.
    pub source_system: String,
    /// Protocol version of this packet.
    pub version: String,
    /// Learned parameter values.
    pub parameters: HashMap<String, f64>,
    /// Summary performance metrics from the source system.
    pub performance_summary: HashMap<String, f64>,
    /// Named patterns or strategies discovered.
    pub patterns: Vec<String>,
    /// Confidence score (0.0 to 1.0) in the quality of this knowledge.
    pub confidence: f64,
    /// Unix timestamp (seconds) when this packet was created.
    pub created_at_secs: u64,
    /// Human-readable description of this knowledge packet.
    pub description: String,
}

/// Result of importing a knowledge packet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferResult {
    /// Number of parameters that were applied.
    pub applied_params: usize,
    /// Number of parameters that were skipped (e.g. out of range or unknown).
    pub skipped_params: usize,
    /// Confidence of the imported packet.
    pub confidence: f64,
    /// Any warnings generated during import.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Inner {
    config: TransferConfig,
    received_packets: Vec<KnowledgePacket>,
    exported_packets: Vec<KnowledgePacket>,
    applied_results: Vec<TransferResult>,
    next_id: u64,
}

// ---------------------------------------------------------------------------
// TransferLearning
// ---------------------------------------------------------------------------

/// Cross-system knowledge transfer manager.
///
/// Cheap to clone -- all clones share the same inner state via `Arc<Mutex<_>>`.
#[derive(Debug, Clone)]
pub struct TransferLearning {
    inner: Arc<Mutex<Inner>>,
}

impl TransferLearning {
    /// Create a new `TransferLearning` instance with the given configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: TransferConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                config,
                received_packets: Vec::new(),
                exported_packets: Vec::new(),
                applied_results: Vec::new(),
                next_id: 1,
            })),
        }
    }

    /// Create a new instance with default configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(TransferConfig::default())
    }

    /// Export a knowledge packet from the local system.
    ///
    /// Creates a [`KnowledgePacket`] from the provided data and stores it in
    /// the exported packets list.
    ///
    /// # Arguments
    /// * `source_system` -- Identifier for the local system
    /// * `parameters` -- Learned parameter values to export
    /// * `performance` -- Performance metrics summary
    /// * `patterns` -- Named patterns or strategies
    /// * `confidence` -- Confidence score (0.0 to 1.0)
    /// * `description` -- Human-readable description
    ///
    /// # Errors
    /// - [`TransferError::InvalidPacket`] if confidence is out of [0.0, 1.0].
    /// - [`TransferError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn export_knowledge(
        &self,
        source_system: &str,
        parameters: HashMap<String, f64>,
        performance: HashMap<String, f64>,
        patterns: Vec<String>,
        confidence: f64,
        description: &str,
    ) -> Result<KnowledgePacket, TransferError> {
        if !(0.0..=1.0).contains(&confidence) {
            return Err(TransferError::InvalidPacket(format!(
                "confidence {confidence} must be in [0.0, 1.0]"
            )));
        }

        let mut inner = self.inner.lock().map_err(|_| TransferError::LockPoisoned)?;
        let id = format!("pkt-{}", inner.next_id);
        inner.next_id += 1;

        let packet = KnowledgePacket {
            id: id.clone(),
            source_system: source_system.to_string(),
            version: inner.config.version.clone(),
            parameters,
            performance_summary: performance,
            patterns,
            confidence,
            created_at_secs: current_timestamp_secs(),
            description: description.to_string(),
        };

        // Enforce max_packets
        let max = inner.config.max_packets;
        if inner.exported_packets.len() >= max {
            inner.exported_packets.remove(0);
        }
        inner.exported_packets.push(packet.clone());

        Ok(packet)
    }

    /// Import a knowledge packet from another system.
    ///
    /// Validates the packet version and confidence, then stores it and
    /// returns a [`TransferResult`] describing what was applied.
    ///
    /// # Errors
    /// - [`TransferError::IncompatibleVersion`] if the packet version does not match.
    /// - [`TransferError::LowConfidence`] if confidence is below minimum.
    /// - [`TransferError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn import_packet(&self, packet: KnowledgePacket) -> Result<TransferResult, TransferError> {
        let mut inner = self.inner.lock().map_err(|_| TransferError::LockPoisoned)?;

        // Version check
        if packet.version != inner.config.version {
            return Err(TransferError::IncompatibleVersion {
                expected: inner.config.version.clone(),
                got: packet.version.clone(),
            });
        }

        // Confidence check
        if packet.confidence < inner.config.min_confidence {
            return Err(TransferError::LowConfidence {
                confidence: packet.confidence,
                minimum: inner.config.min_confidence,
            });
        }

        let mut warnings = Vec::new();
        let applied_params = packet.parameters.len();
        let skipped_params = 0;

        // Check age
        let now = current_timestamp_secs();
        let age = now.saturating_sub(packet.created_at_secs);
        if age > inner.config.max_packet_age_secs {
            warnings.push(format!(
                "packet is {age}s old, exceeds max age of {}s",
                inner.config.max_packet_age_secs
            ));
        }

        // Store received packet
        let max = inner.config.max_packets;
        if inner.received_packets.len() >= max {
            inner.received_packets.remove(0);
        }
        inner.received_packets.push(packet.clone());

        let result = TransferResult {
            applied_params,
            skipped_params,
            confidence: packet.confidence,
            warnings,
        };

        inner.applied_results.push(result.clone());
        Ok(result)
    }

    /// Retrieve a received packet by ID.
    ///
    /// # Errors
    /// - [`TransferError::PacketNotFound`] if the ID is unknown.
    /// - [`TransferError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get_packet(&self, id: &str) -> Result<KnowledgePacket, TransferError> {
        let inner = self.inner.lock().map_err(|_| TransferError::LockPoisoned)?;
        inner
            .received_packets
            .iter()
            .chain(inner.exported_packets.iter())
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| TransferError::PacketNotFound(id.to_string()))
    }

    /// Return all received packets.
    ///
    /// # Panics
    /// This function never panics.
    pub fn received_packets(&self) -> Vec<KnowledgePacket> {
        match self.inner.lock() {
            Ok(g) => g.received_packets.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Return all exported packets.
    ///
    /// # Panics
    /// This function never panics.
    pub fn exported_packets(&self) -> Vec<KnowledgePacket> {
        match self.inner.lock() {
            Ok(g) => g.exported_packets.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Return all transfer results from imports.
    ///
    /// # Panics
    /// This function never panics.
    pub fn applied_results(&self) -> Vec<TransferResult> {
        match self.inner.lock() {
            Ok(g) => g.applied_results.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Return the parameters from the highest-confidence received packet.
    ///
    /// Returns `None` if no packets have been received.
    ///
    /// # Panics
    /// This function never panics.
    pub fn best_parameters(&self) -> Option<HashMap<String, f64>> {
        match self.inner.lock() {
            Ok(g) => g
                .received_packets
                .iter()
                .max_by(|a, b| {
                    a.confidence
                        .partial_cmp(&b.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|p| p.parameters.clone()),
            Err(_) => None,
        }
    }

    /// Return the total number of received and exported packets.
    ///
    /// # Panics
    /// This function never panics.
    pub fn packet_count(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.received_packets.len() + g.exported_packets.len(),
            Err(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_params() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("learning_rate".to_string(), 0.01);
        m.insert("batch_size".to_string(), 32.0);
        m
    }

    fn sample_perf() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("throughput".to_string(), 1500.0);
        m.insert("latency_p99_ms".to_string(), 12.0);
        m
    }

    #[test]
    fn test_export_creates_packet() {
        let tl = TransferLearning::with_defaults();
        let pkt = tl
            .export_knowledge("sys-a", sample_params(), sample_perf(), vec![], 0.9, "test")
            .unwrap();
        assert!(pkt.id.starts_with("pkt-"));
        assert_eq!(pkt.source_system, "sys-a");
        assert_eq!(pkt.version, "1.0");
    }

    #[test]
    fn test_export_invalid_confidence() {
        let tl = TransferLearning::with_defaults();
        let err = tl
            .export_knowledge("sys-a", sample_params(), sample_perf(), vec![], 1.5, "bad")
            .unwrap_err();
        assert!(matches!(err, TransferError::InvalidPacket(_)));
    }

    #[test]
    fn test_export_negative_confidence() {
        let tl = TransferLearning::with_defaults();
        let err = tl
            .export_knowledge("sys-a", sample_params(), sample_perf(), vec![], -0.1, "bad")
            .unwrap_err();
        assert!(matches!(err, TransferError::InvalidPacket(_)));
    }

    #[test]
    fn test_import_validates_version() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-ext-1".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: sample_perf(),
            patterns: vec!["pattern-a".to_string()],
            confidence: 0.85,
            created_at_secs: current_timestamp_secs(),
            description: "good packet".to_string(),
        };
        let result = tl.import_packet(pkt).unwrap();
        assert_eq!(result.applied_params, 2);
        assert_eq!(result.skipped_params, 0);
    }

    #[test]
    fn test_import_rejects_incompatible_version() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-ext-1".to_string(),
            source_system: "other".to_string(),
            version: "2.0".to_string(),
            parameters: sample_params(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.9,
            created_at_secs: current_timestamp_secs(),
            description: "wrong version".to_string(),
        };
        let err = tl.import_packet(pkt).unwrap_err();
        assert!(matches!(err, TransferError::IncompatibleVersion { .. }));
    }

    #[test]
    fn test_import_rejects_low_confidence() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-ext-1".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.3,
            created_at_secs: current_timestamp_secs(),
            description: "low confidence".to_string(),
        };
        let err = tl.import_packet(pkt).unwrap_err();
        assert!(matches!(err, TransferError::LowConfidence { .. }));
    }

    #[test]
    fn test_get_packet_received() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-find-me".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.9,
            created_at_secs: current_timestamp_secs(),
            description: "findable".to_string(),
        };
        tl.import_packet(pkt).unwrap();
        let found = tl.get_packet("pkt-find-me").unwrap();
        assert_eq!(found.source_system, "other");
    }

    #[test]
    fn test_get_packet_exported() {
        let tl = TransferLearning::with_defaults();
        let pkt = tl
            .export_knowledge("sys-a", sample_params(), sample_perf(), vec![], 0.8, "exp")
            .unwrap();
        let found = tl.get_packet(&pkt.id).unwrap();
        assert_eq!(found.source_system, "sys-a");
    }

    #[test]
    fn test_get_packet_not_found() {
        let tl = TransferLearning::with_defaults();
        let err = tl.get_packet("nonexistent").unwrap_err();
        assert!(matches!(err, TransferError::PacketNotFound(_)));
    }

    #[test]
    fn test_received_packets() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-1".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.9,
            created_at_secs: current_timestamp_secs(),
            description: "test".to_string(),
        };
        tl.import_packet(pkt).unwrap();
        assert_eq!(tl.received_packets().len(), 1);
    }

    #[test]
    fn test_exported_packets() {
        let tl = TransferLearning::with_defaults();
        tl.export_knowledge("sys-a", sample_params(), sample_perf(), vec![], 0.8, "e")
            .unwrap();
        assert_eq!(tl.exported_packets().len(), 1);
    }

    #[test]
    fn test_applied_results() {
        let tl = TransferLearning::with_defaults();
        let pkt = KnowledgePacket {
            id: "pkt-r".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.9,
            created_at_secs: current_timestamp_secs(),
            description: "test".to_string(),
        };
        tl.import_packet(pkt).unwrap();
        let results = tl.applied_results();
        assert_eq!(results.len(), 1);
        assert!((results[0].confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn test_best_parameters() {
        let tl = TransferLearning::with_defaults();
        // Import two packets with different confidences
        let pkt1 = KnowledgePacket {
            id: "pkt-low".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: {
                let mut m = HashMap::new();
                m.insert("lr".to_string(), 0.01);
                m
            },
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.75,
            created_at_secs: current_timestamp_secs(),
            description: "low".to_string(),
        };
        let pkt2 = KnowledgePacket {
            id: "pkt-high".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: {
                let mut m = HashMap::new();
                m.insert("lr".to_string(), 0.05);
                m
            },
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.95,
            created_at_secs: current_timestamp_secs(),
            description: "high".to_string(),
        };
        tl.import_packet(pkt1).unwrap();
        tl.import_packet(pkt2).unwrap();
        let best = tl.best_parameters().unwrap();
        assert!((best["lr"] - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_best_parameters_none_when_empty() {
        let tl = TransferLearning::with_defaults();
        assert!(tl.best_parameters().is_none());
    }

    #[test]
    fn test_packet_count() {
        let tl = TransferLearning::with_defaults();
        assert_eq!(tl.packet_count(), 0);
        tl.export_knowledge("sys-a", sample_params(), sample_perf(), vec![], 0.8, "e")
            .unwrap();
        assert_eq!(tl.packet_count(), 1);
        let pkt = KnowledgePacket {
            id: "pkt-i".to_string(),
            source_system: "other".to_string(),
            version: "1.0".to_string(),
            parameters: HashMap::new(),
            performance_summary: HashMap::new(),
            patterns: vec![],
            confidence: 0.9,
            created_at_secs: current_timestamp_secs(),
            description: "test".to_string(),
        };
        tl.import_packet(pkt).unwrap();
        assert_eq!(tl.packet_count(), 2);
    }

    #[test]
    fn test_config_defaults() {
        let config = TransferConfig::default();
        assert_eq!(config.version, "1.0");
        assert!((config.min_confidence - 0.7).abs() < f64::EPSILON);
        assert_eq!(config.max_packet_age_secs, 86400);
        assert_eq!(config.max_packets, 1000);
    }

    #[test]
    fn test_serialization_config() {
        let config = TransferConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TransferConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, "1.0");
    }

    #[test]
    fn test_serialization_packet() {
        let pkt = KnowledgePacket {
            id: "pkt-ser".to_string(),
            source_system: "sys".to_string(),
            version: "1.0".to_string(),
            parameters: sample_params(),
            performance_summary: sample_perf(),
            patterns: vec!["p1".to_string()],
            confidence: 0.88,
            created_at_secs: 1000,
            description: "serialize test".to_string(),
        };
        let json = serde_json::to_string(&pkt).unwrap();
        let back: KnowledgePacket = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "pkt-ser");
        assert_eq!(back.patterns, vec!["p1".to_string()]);
    }

    #[test]
    fn test_serialization_result() {
        let result = TransferResult {
            applied_params: 5,
            skipped_params: 1,
            confidence: 0.9,
            warnings: vec!["old packet".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TransferResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.applied_params, 5);
    }

    #[test]
    fn test_clone_shares_state() {
        let tl = TransferLearning::with_defaults();
        let tl2 = tl.clone();
        tl.export_knowledge("sys", sample_params(), sample_perf(), vec![], 0.8, "t")
            .unwrap();
        assert_eq!(tl2.packet_count(), 1);
    }

    #[test]
    fn test_error_display() {
        let err = TransferError::IncompatibleVersion {
            expected: "1.0".to_string(),
            got: "2.0".to_string(),
        };
        assert!(err.to_string().contains("1.0"));
        assert!(err.to_string().contains("2.0"));

        let err = TransferError::LowConfidence {
            confidence: 0.3,
            minimum: 0.7,
        };
        assert!(err.to_string().contains("0.3"));

        let err = TransferError::PacketNotFound("x".to_string());
        assert!(err.to_string().contains("x"));
    }

    #[test]
    fn test_export_with_patterns() {
        let tl = TransferLearning::with_defaults();
        let patterns = vec!["batch-first".to_string(), "low-latency".to_string()];
        let pkt = tl
            .export_knowledge(
                "sys-a",
                sample_params(),
                sample_perf(),
                patterns.clone(),
                0.9,
                "patterns test",
            )
            .unwrap();
        assert_eq!(pkt.patterns, patterns);
    }
}
