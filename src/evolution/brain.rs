//! # Stage: ConsensusBrain (Task 4.1 -- Multi-Node Consensus)
//!
//! ## Responsibility
//! Coordinate parameter decisions across multiple orchestrator nodes via a
//! simple voting protocol. Each node proposes parameter changes; changes are
//! applied only when a quorum agrees.
//!
//! ## Guarantees
//! - Thread-safe: all operations use `Arc<Mutex<Inner>>`
//! - Deterministic quorum: `ceil(known_nodes * quorum_fraction)` votes required
//! - Bounded: `max_active_proposals` limits concurrent proposals
//! - Non-panicking: every public method returns `Result`
//!
//! ## NOT Responsible For
//! - Network transport (uses abstract message passing)
//! - Leader election (all nodes are equal peers)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors specific to the consensus brain.
#[derive(Debug, Error)]
pub enum BrainError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("consensus brain lock poisoned")]
    LockPoisoned,

    /// The requested proposal was not found.
    #[error("proposal not found: {0}")]
    ProposalNotFound(String),

    /// Not enough votes to reach quorum.
    #[error("quorum not reached: {votes} votes, {needed} needed")]
    QuorumNotReached {
        /// Number of votes received.
        votes: usize,
        /// Number of votes required.
        needed: usize,
    },

    /// A voter has already cast a vote on this proposal.
    #[error("duplicate vote by '{voter}' on proposal '{proposal_id}'")]
    DuplicateVote {
        /// The voter who attempted a duplicate vote.
        voter: String,
        /// The proposal that received the duplicate vote.
        proposal_id: String,
    },

    /// Maximum number of active proposals has been reached.
    #[error("max active proposals reached: {0}")]
    MaxProposalsReached(usize),
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Status of a consensus proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    /// Awaiting votes from peers.
    Pending,
    /// Quorum approved the change.
    Approved,
    /// Quorum rejected the change.
    Rejected,
    /// Proposal expired before quorum was reached.
    TimedOut,
    /// Approved proposal has been applied to the running system.
    Applied,
}

/// Configuration for the consensus brain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusConfig {
    /// Fraction of known nodes required for quorum (e.g. 0.5 = majority).
    pub quorum_fraction: f64,
    /// Seconds before a pending proposal times out.
    pub proposal_timeout_secs: u64,
    /// Maximum number of concurrently active proposals.
    pub max_active_proposals: usize,
    /// Identifier for the local node.
    pub node_id: String,
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        Self {
            quorum_fraction: 0.5,
            proposal_timeout_secs: 30,
            max_active_proposals: 10,
            node_id: String::from("node-default"),
        }
    }
}

/// A single vote on a consensus proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusVote {
    /// Identifier of the voting node.
    pub voter: String,
    /// Whether this vote approves the proposal.
    pub approve: bool,
    /// Optional textual reason for the vote.
    pub reason: Option<String>,
    /// Unix timestamp (seconds) when the vote was cast.
    pub timestamp_secs: u64,
}

/// A proposal to change a tunable parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusProposal {
    /// Unique proposal identifier.
    pub id: String,
    /// Node that created this proposal.
    pub proposer: String,
    /// Name of the parameter to change.
    pub parameter: String,
    /// Current value of the parameter.
    pub old_value: f64,
    /// Proposed new value of the parameter.
    pub new_value: f64,
    /// Human-readable reason for the change.
    pub reason: String,
    /// Votes received so far.
    pub votes: Vec<ConsensusVote>,
    /// Current status of the proposal.
    pub status: ProposalStatus,
    /// Unix timestamp (seconds) when the proposal was created.
    pub created_at_secs: u64,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Inner {
    config: ConsensusConfig,
    proposals: HashMap<String, ConsensusProposal>,
    known_nodes: Vec<String>,
    applied_history: Vec<ConsensusProposal>,
    max_history: usize,
    next_id: u64,
}

// ---------------------------------------------------------------------------
// ConsensusBrain
// ---------------------------------------------------------------------------

/// Multi-node consensus coordinator for parameter changes.
///
/// Cheap to clone -- all clones share the same inner state via `Arc<Mutex<_>>`.
#[derive(Debug, Clone)]
pub struct ConsensusBrain {
    inner: Arc<Mutex<Inner>>,
}

impl ConsensusBrain {
    /// Create a new `ConsensusBrain` with the given configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: ConsensusConfig) -> Self {
        let node_id = config.node_id.clone();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                config,
                proposals: HashMap::new(),
                known_nodes: vec![node_id],
                applied_history: Vec::new(),
                max_history: 1000,
                next_id: 1,
            })),
        }
    }

    /// Register a peer node so it counts toward quorum calculations.
    ///
    /// # Errors
    /// Returns [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn register_node(&self, node_id: &str) -> Result<(), BrainError> {
        let mut inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;
        if !inner.known_nodes.contains(&node_id.to_string()) {
            inner.known_nodes.push(node_id.to_string());
        }
        Ok(())
    }

    /// Submit a new proposal to change a parameter.
    ///
    /// # Errors
    /// - [`BrainError::MaxProposalsReached`] if too many proposals are pending.
    /// - [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn propose(
        &self,
        parameter: &str,
        old_value: f64,
        new_value: f64,
        reason: &str,
    ) -> Result<String, BrainError> {
        let mut inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;

        let active_count = inner
            .proposals
            .values()
            .filter(|p| p.status == ProposalStatus::Pending)
            .count();
        if active_count >= inner.config.max_active_proposals {
            return Err(BrainError::MaxProposalsReached(
                inner.config.max_active_proposals,
            ));
        }

        let id = format!("proposal-{}", inner.next_id);
        inner.next_id += 1;

        let proposal = ConsensusProposal {
            id: id.clone(),
            proposer: inner.config.node_id.clone(),
            parameter: parameter.to_string(),
            old_value,
            new_value,
            reason: reason.to_string(),
            votes: Vec::new(),
            status: ProposalStatus::Pending,
            created_at_secs: current_timestamp_secs(),
        };

        inner.proposals.insert(id.clone(), proposal);
        Ok(id)
    }

    /// Cast a vote on an existing proposal.
    ///
    /// After recording the vote the method checks whether quorum has been
    /// reached and updates the proposal status accordingly.
    ///
    /// # Errors
    /// - [`BrainError::ProposalNotFound`] if the proposal ID is unknown.
    /// - [`BrainError::DuplicateVote`] if the voter already voted.
    /// - [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn vote(
        &self,
        proposal_id: &str,
        voter: &str,
        approve: bool,
        reason: Option<&str>,
    ) -> Result<ProposalStatus, BrainError> {
        let mut inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;

        // Extract values before mutable borrow of proposal to avoid borrow conflict.
        let known_nodes_len = inner.known_nodes.len();
        let quorum_fraction = inner.config.quorum_fraction;

        let proposal = inner
            .proposals
            .get_mut(proposal_id)
            .ok_or_else(|| BrainError::ProposalNotFound(proposal_id.to_string()))?;

        if proposal.votes.iter().any(|v| v.voter == voter) {
            return Err(BrainError::DuplicateVote {
                voter: voter.to_string(),
                proposal_id: proposal_id.to_string(),
            });
        }

        proposal.votes.push(ConsensusVote {
            voter: voter.to_string(),
            approve,
            reason: reason.map(String::from),
            timestamp_secs: current_timestamp_secs(),
        });

        // Check quorum
        let needed = quorum_size(known_nodes_len, quorum_fraction);
        let approve_count = proposal.votes.iter().filter(|v| v.approve).count();
        let reject_count = proposal.votes.iter().filter(|v| !v.approve).count();

        if approve_count >= needed {
            proposal.status = ProposalStatus::Approved;
        } else if reject_count >= needed {
            proposal.status = ProposalStatus::Rejected;
        }

        Ok(proposal.status.clone())
    }

    /// Retrieve a proposal by ID.
    ///
    /// # Errors
    /// - [`BrainError::ProposalNotFound`] if the proposal ID is unknown.
    /// - [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get_proposal(&self, proposal_id: &str) -> Result<ConsensusProposal, BrainError> {
        let inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;
        inner
            .proposals
            .get(proposal_id)
            .cloned()
            .ok_or_else(|| BrainError::ProposalNotFound(proposal_id.to_string()))
    }

    /// Return all proposals that are currently pending.
    ///
    /// # Panics
    /// This function never panics.
    pub fn pending_proposals(&self) -> Vec<ConsensusProposal> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        inner
            .proposals
            .values()
            .filter(|p| p.status == ProposalStatus::Pending)
            .cloned()
            .collect()
    }

    /// Return all proposals that have been applied.
    ///
    /// # Panics
    /// This function never panics.
    pub fn applied_proposals(&self) -> Vec<ConsensusProposal> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        inner.applied_history.clone()
    }

    /// Transition an approved proposal to the `Applied` state.
    ///
    /// # Errors
    /// - [`BrainError::ProposalNotFound`] if the proposal ID is unknown.
    /// - [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn mark_applied(&self, proposal_id: &str) -> Result<(), BrainError> {
        let mut inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;
        let proposal = inner
            .proposals
            .get_mut(proposal_id)
            .ok_or_else(|| BrainError::ProposalNotFound(proposal_id.to_string()))?;

        proposal.status = ProposalStatus::Applied;
        let applied = proposal.clone();

        if inner.applied_history.len() >= inner.max_history {
            inner.applied_history.remove(0);
        }
        inner.applied_history.push(applied);
        Ok(())
    }

    /// Expire pending proposals that have exceeded the configured timeout.
    ///
    /// Returns the IDs of proposals that were timed out.
    ///
    /// # Errors
    /// Returns [`BrainError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn check_timeouts(&self) -> Result<Vec<String>, BrainError> {
        let mut inner = self.inner.lock().map_err(|_| BrainError::LockPoisoned)?;
        let now = current_timestamp_secs();
        let timeout = inner.config.proposal_timeout_secs;
        let mut expired = Vec::new();

        for proposal in inner.proposals.values_mut() {
            if proposal.status == ProposalStatus::Pending
                && now.saturating_sub(proposal.created_at_secs) >= timeout
            {
                proposal.status = ProposalStatus::TimedOut;
                expired.push(proposal.id.clone());
            }
        }

        Ok(expired)
    }

    /// Return the number of known peer nodes (including self).
    ///
    /// # Panics
    /// This function never panics.
    pub fn node_count(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.known_nodes.len(),
            Err(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn quorum_size(total_nodes: usize, fraction: f64) -> usize {
    let raw = (total_nodes as f64 * fraction).ceil() as usize;
    if raw == 0 {
        1
    } else {
        raw
    }
}

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

    fn default_brain() -> ConsensusBrain {
        ConsensusBrain::new(ConsensusConfig {
            quorum_fraction: 0.5,
            proposal_timeout_secs: 30,
            max_active_proposals: 10,
            node_id: "node-1".to_string(),
        })
    }

    #[test]
    fn test_propose_returns_id() {
        let brain = default_brain();
        let id = brain
            .propose("param_a", 1.0, 2.0, "improve latency")
            .unwrap();
        assert!(id.starts_with("proposal-"));
    }

    #[test]
    fn test_propose_sequential_ids() {
        let brain = default_brain();
        let id1 = brain.propose("a", 1.0, 2.0, "r1").unwrap();
        let id2 = brain.propose("b", 3.0, 4.0, "r2").unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_vote_approve_reaches_quorum() {
        let brain = default_brain();
        // Only 1 node, quorum = ceil(1 * 0.5) = 1
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        let status = brain.vote(&id, "node-1", true, None).unwrap();
        assert_eq!(status, ProposalStatus::Approved);
    }

    #[test]
    fn test_vote_reject_reaches_quorum() {
        let brain = default_brain();
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        let status = brain.vote(&id, "node-1", false, Some("bad idea")).unwrap();
        assert_eq!(status, ProposalStatus::Rejected);
    }

    #[test]
    fn test_quorum_with_multiple_nodes() {
        let brain = default_brain();
        brain.register_node("node-2").unwrap();
        brain.register_node("node-3").unwrap();
        // 3 nodes, quorum = ceil(3 * 0.5) = 2
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        let s1 = brain.vote(&id, "node-1", true, None).unwrap();
        assert_eq!(s1, ProposalStatus::Pending);
        let s2 = brain.vote(&id, "node-2", true, None).unwrap();
        assert_eq!(s2, ProposalStatus::Approved);
    }

    #[test]
    fn test_duplicate_vote_error() {
        let brain = default_brain();
        brain.register_node("node-2").unwrap();
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        brain.vote(&id, "node-1", true, None).unwrap();
        let err = brain.vote(&id, "node-1", false, None).unwrap_err();
        assert!(matches!(err, BrainError::DuplicateVote { .. }));
    }

    #[test]
    fn test_proposal_not_found() {
        let brain = default_brain();
        let err = brain.vote("nonexistent", "node-1", true, None).unwrap_err();
        assert!(matches!(err, BrainError::ProposalNotFound(_)));
    }

    #[test]
    fn test_get_proposal() {
        let brain = default_brain();
        let id = brain.propose("p", 1.0, 2.0, "reason").unwrap();
        let proposal = brain.get_proposal(&id).unwrap();
        assert_eq!(proposal.parameter, "p");
        assert_eq!(proposal.old_value, 1.0);
        assert_eq!(proposal.new_value, 2.0);
        assert_eq!(proposal.reason, "reason");
        assert_eq!(proposal.status, ProposalStatus::Pending);
    }

    #[test]
    fn test_get_proposal_not_found() {
        let brain = default_brain();
        let err = brain.get_proposal("nope").unwrap_err();
        assert!(matches!(err, BrainError::ProposalNotFound(_)));
    }

    #[test]
    fn test_timeout_expiration() {
        let brain = ConsensusBrain::new(ConsensusConfig {
            quorum_fraction: 0.5,
            proposal_timeout_secs: 0, // immediate timeout
            max_active_proposals: 10,
            node_id: "node-1".to_string(),
        });
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        let expired = brain.check_timeouts().unwrap();
        assert!(expired.contains(&id));

        let proposal = brain.get_proposal(&id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::TimedOut);
    }

    #[test]
    fn test_register_node_increments_count() {
        let brain = default_brain();
        assert_eq!(brain.node_count(), 1);
        brain.register_node("node-2").unwrap();
        assert_eq!(brain.node_count(), 2);
    }

    #[test]
    fn test_register_node_idempotent() {
        let brain = default_brain();
        brain.register_node("node-2").unwrap();
        brain.register_node("node-2").unwrap();
        assert_eq!(brain.node_count(), 2);
    }

    #[test]
    fn test_mark_applied() {
        let brain = default_brain();
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        brain.vote(&id, "node-1", true, None).unwrap();
        brain.mark_applied(&id).unwrap();
        let proposal = brain.get_proposal(&id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Applied);
    }

    #[test]
    fn test_pending_proposals() {
        let brain = default_brain();
        brain.propose("a", 1.0, 2.0, "r1").unwrap();
        brain.propose("b", 3.0, 4.0, "r2").unwrap();
        let pending = brain.pending_proposals();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_applied_history() {
        let brain = default_brain();
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        brain.vote(&id, "node-1", true, None).unwrap();
        brain.mark_applied(&id).unwrap();
        let applied = brain.applied_proposals();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].id, id);
    }

    #[test]
    fn test_config_defaults() {
        let config = ConsensusConfig::default();
        assert!((config.quorum_fraction - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.proposal_timeout_secs, 30);
        assert_eq!(config.max_active_proposals, 10);
    }

    #[test]
    fn test_clone_shares_state() {
        let brain = default_brain();
        let brain2 = brain.clone();
        brain.propose("x", 0.0, 1.0, "shared").unwrap();
        assert_eq!(brain2.pending_proposals().len(), 1);
    }

    #[test]
    fn test_serialization_config() {
        let config = ConsensusConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConsensusConfig = serde_json::from_str(&json).unwrap();
        assert!((deserialized.quorum_fraction - config.quorum_fraction).abs() < f64::EPSILON);
    }

    #[test]
    fn test_serialization_proposal() {
        let proposal = ConsensusProposal {
            id: "p-1".to_string(),
            proposer: "n1".to_string(),
            parameter: "lr".to_string(),
            old_value: 0.01,
            new_value: 0.02,
            reason: "improve".to_string(),
            votes: vec![],
            status: ProposalStatus::Pending,
            created_at_secs: 100,
        };
        let json = serde_json::to_string(&proposal).unwrap();
        let deserialized: ConsensusProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "p-1");
    }

    #[test]
    fn test_max_proposals_reached() {
        let brain = ConsensusBrain::new(ConsensusConfig {
            max_active_proposals: 2,
            ..ConsensusConfig::default()
        });
        brain.propose("a", 0.0, 1.0, "r").unwrap();
        brain.propose("b", 0.0, 1.0, "r").unwrap();
        let err = brain.propose("c", 0.0, 1.0, "r").unwrap_err();
        assert!(matches!(err, BrainError::MaxProposalsReached(2)));
    }

    #[test]
    fn test_quorum_size_helper() {
        assert_eq!(quorum_size(1, 0.5), 1);
        assert_eq!(quorum_size(3, 0.5), 2);
        assert_eq!(quorum_size(5, 0.5), 3);
        assert_eq!(quorum_size(5, 0.8), 4);
        assert_eq!(quorum_size(0, 0.5), 1);
    }

    #[test]
    fn test_proposal_status_variants() {
        let statuses = vec![
            ProposalStatus::Pending,
            ProposalStatus::Approved,
            ProposalStatus::Rejected,
            ProposalStatus::TimedOut,
            ProposalStatus::Applied,
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let back: ProposalStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, s);
        }
    }

    #[test]
    fn test_mark_applied_not_found() {
        let brain = default_brain();
        let err = brain.mark_applied("nope").unwrap_err();
        assert!(matches!(err, BrainError::ProposalNotFound(_)));
    }

    #[test]
    fn test_vote_with_reason() {
        let brain = default_brain();
        let id = brain.propose("p", 1.0, 2.0, "test").unwrap();
        brain.vote(&id, "node-1", true, Some("looks good")).unwrap();
        let proposal = brain.get_proposal(&id).unwrap();
        assert_eq!(proposal.votes[0].reason.as_deref(), Some("looks good"));
    }

    #[test]
    fn test_error_display() {
        let err = BrainError::ProposalNotFound("p-1".to_string());
        assert!(err.to_string().contains("p-1"));

        let err = BrainError::QuorumNotReached {
            votes: 1,
            needed: 3,
        };
        assert!(err.to_string().contains("1"));

        let err = BrainError::DuplicateVote {
            voter: "v".to_string(),
            proposal_id: "p".to_string(),
        };
        assert!(err.to_string().contains("duplicate vote"));
    }
}
