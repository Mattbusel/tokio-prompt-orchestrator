//! # Evolution Subsystem (Phase 4)
//!
//! Multi-node distributed evolution capabilities: consensus-based parameter
//! sharing, evolutionary search, plugin architecture, and cross-system learning.
//!
//! ## Feature flag: `evolution`
//! Implies `self-tune`, `intelligence`, and `distributed`. The pipeline operates
//! identically with this feature disabled.
//!
//! ## Module map
//! - [`brain`]    -- multi-node consensus for parameter sharing
//! - [`search`]   -- evolutionary parameter search (genetic algorithms)
//! - [`plugin`]   -- plugin architecture for extensible behaviors
//! - [`transfer`] -- cross-system knowledge transfer

pub mod brain;
pub mod plugin;
pub mod search;
pub mod transfer;

pub use brain::{ConsensusBrain, ConsensusConfig, ConsensusProposal, ConsensusVote};
pub use plugin::{PluginError, PluginManager, PluginManifest};
pub use search::{EvolutionConfig, EvolutionarySearch, Individual, SearchResult};
pub use transfer::{KnowledgePacket, TransferConfig, TransferLearning, TransferResult};
