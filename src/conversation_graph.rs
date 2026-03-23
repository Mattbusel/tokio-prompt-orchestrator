//! DAG-based conversation branching.
//!
//! A [`ConversationGraph`] is a directed acyclic graph (DAG) where each node
//! represents one conversation turn (a role + content pair).  Edges represent
//! the "follows from" relationship between turns.  The graph enforces the
//! acyclicity invariant on every [`ConversationGraph::add_edge`] call via a
//! depth-first reachability check.

use std::collections::{HashMap, HashSet, VecDeque};

/// Opaque node identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// A single node in the conversation DAG.
#[derive(Debug, Clone)]
pub struct ConversationNode {
    /// Unique identifier of this node.
    pub id: NodeId,
    /// Speaker role, e.g. `"user"`, `"assistant"`, or `"system"`.
    pub role: String,
    /// Text content of the turn.
    pub content: String,
    /// Token count for this turn.
    pub tokens: usize,
    /// Arbitrary key-value metadata.
    pub metadata: HashMap<String, String>,
}

/// Errors that can be returned by graph operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphError {
    /// The requested edge would introduce a cycle into the DAG.
    #[error("adding this edge would create a cycle")]
    CycleDetected,
    /// One of the referenced node IDs does not exist in the graph.
    #[error("node {0:?} not found")]
    NodeNotFound(NodeId),
    /// The edge specification is logically invalid (e.g. self-loop).
    #[error("invalid edge: {0}")]
    InvalidEdge(String),
}

/// Directed acyclic graph of conversation nodes.
///
/// # Invariants
///
/// - Every `NodeId` returned by [`add_node`] is unique and stable for the
///   lifetime of the graph.
/// - [`add_edge`] rejects any edge that would create a cycle.
/// - [`linearize`] always returns a valid topological ordering.
#[derive(Debug, Default)]
pub struct ConversationGraph {
    /// All nodes indexed by their ID.
    nodes: HashMap<NodeId, ConversationNode>,
    /// Forward adjacency list: node -> list of successor nodes.
    edges: HashMap<NodeId, Vec<NodeId>>,
    /// Monotonically increasing counter for generating fresh `NodeId`s.
    next_id: u64,
}

impl ConversationGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    fn fresh_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Return `true` if `target` is reachable from `start` by following forward
    /// edges (DFS).  Used to detect would-be cycles before inserting an edge.
    fn is_reachable(&self, start: NodeId, target: NodeId) -> bool {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut stack: Vec<NodeId> = vec![start];
        while let Some(current) = stack.pop() {
            if current == target {
                return true;
            }
            if visited.insert(current) {
                if let Some(succs) = self.edges.get(&current) {
                    stack.extend_from_slice(succs);
                }
            }
        }
        false
    }

    // -------------------------------------------------------------------------
    // Public API
    // -------------------------------------------------------------------------

    /// Add a new node to the graph and return its [`NodeId`].
    pub fn add_node(&mut self, role: impl Into<String>, content: impl Into<String>, tokens: usize) -> NodeId {
        let id = self.fresh_id();
        let node = ConversationNode {
            id,
            role: role.into(),
            content: content.into(),
            tokens,
            metadata: HashMap::new(),
        };
        self.nodes.insert(id, node);
        self.edges.entry(id).or_default();
        id
    }

    /// Add a directed edge `from -> to`.
    ///
    /// # Errors
    ///
    /// - [`GraphError::NodeNotFound`] if either `from` or `to` is unknown.
    /// - [`GraphError::InvalidEdge`] if `from == to` (self-loop).
    /// - [`GraphError::CycleDetected`] if the edge would create a cycle.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) -> Result<(), GraphError> {
        if !self.nodes.contains_key(&from) {
            return Err(GraphError::NodeNotFound(from));
        }
        if !self.nodes.contains_key(&to) {
            return Err(GraphError::NodeNotFound(to));
        }
        if from == to {
            return Err(GraphError::InvalidEdge("self-loop".into()));
        }
        // Adding from->to creates a cycle iff `from` is already reachable from `to`.
        if self.is_reachable(to, from) {
            return Err(GraphError::CycleDetected);
        }
        self.edges.entry(from).or_default().push(to);
        Ok(())
    }

    /// Clone the node at `node_id`, attach the clone as a child, and return
    /// the clone's [`NodeId`].
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::NodeNotFound`] if `node_id` is unknown.
    pub fn branch_from(&mut self, node_id: NodeId) -> Result<NodeId, GraphError> {
        let original = self
            .nodes
            .get(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?
            .clone();
        let new_id = self.fresh_id();
        let clone = ConversationNode {
            id: new_id,
            role: original.role,
            content: original.content,
            tokens: original.tokens,
            metadata: original.metadata,
        };
        self.nodes.insert(new_id, clone);
        self.edges.entry(new_id).or_default();
        // Attach original -> clone
        self.edges.entry(node_id).or_default().push(new_id);
        Ok(new_id)
    }

    /// Create a merge node that references both `path_a` and `path_b`.
    ///
    /// The merge node has role `"merge"` and empty content.  Edges are added
    /// from the last node of each path to the merge node.
    ///
    /// # Errors
    ///
    /// - [`GraphError::InvalidEdge`] if either path is empty.
    /// - Propagates errors from [`add_edge`].
    pub fn merge_paths(
        &mut self,
        path_a: &[NodeId],
        path_b: &[NodeId],
    ) -> Result<NodeId, GraphError> {
        if path_a.is_empty() || path_b.is_empty() {
            return Err(GraphError::InvalidEdge("paths must be non-empty".into()));
        }
        let merge_id = self.add_node("merge", "", 0);
        let tail_a = *path_a.last().expect("checked non-empty");
        let tail_b = *path_b.last().expect("checked non-empty");
        self.add_edge(tail_a, merge_id)?;
        if tail_b != tail_a {
            self.add_edge(tail_b, merge_id)?;
        }
        Ok(merge_id)
    }

    /// Topological sort of all nodes reachable from `root` using Kahn's algorithm.
    ///
    /// Returns nodes in breadth-first topological order.
    ///
    /// # Errors
    ///
    /// - [`GraphError::NodeNotFound`] if `root` is unknown.
    /// - [`GraphError::CycleDetected`] if the reachable subgraph is not a DAG
    ///   (should not happen if all edges were added through [`add_edge`]).
    pub fn linearize(&self, root: NodeId) -> Result<Vec<NodeId>, GraphError> {
        if !self.nodes.contains_key(&root) {
            return Err(GraphError::NodeNotFound(root));
        }

        // Collect the subgraph reachable from root.
        let mut reachable: HashSet<NodeId> = HashSet::new();
        let mut dfs_stack = vec![root];
        while let Some(n) = dfs_stack.pop() {
            if reachable.insert(n) {
                if let Some(succs) = self.edges.get(&n) {
                    dfs_stack.extend_from_slice(succs);
                }
            }
        }

        // Build in-degree counts restricted to reachable nodes.
        let mut in_degree: HashMap<NodeId, usize> = reachable.iter().map(|&n| (n, 0)).collect();
        for &n in &reachable {
            if let Some(succs) = self.edges.get(&n) {
                for &s in succs {
                    if reachable.contains(&s) {
                        *in_degree.entry(s).or_insert(0) += 1;
                    }
                }
            }
        }

        // Kahn's BFS starting from nodes with in-degree 0 that are reachable.
        let mut queue: VecDeque<NodeId> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&n, _)| n)
            .collect();
        let mut order: Vec<NodeId> = Vec::with_capacity(reachable.len());

        while let Some(n) = queue.pop_front() {
            order.push(n);
            if let Some(succs) = self.edges.get(&n) {
                for &s in succs {
                    if reachable.contains(&s) {
                        let d = in_degree.entry(s).or_insert(0);
                        *d = d.saturating_sub(1);
                        if *d == 0 {
                            queue.push_back(s);
                        }
                    }
                }
            }
        }

        if order.len() != reachable.len() {
            return Err(GraphError::CycleDetected);
        }
        Ok(order)
    }

    /// Sum the token counts of all nodes in `path`.
    ///
    /// Unknown node IDs are silently skipped.
    pub fn path_tokens(&self, path: &[NodeId]) -> usize {
        path.iter()
            .filter_map(|id| self.nodes.get(id))
            .map(|n| n.tokens)
            .sum()
    }

    /// Borrow the node for `id`, if it exists.
    pub fn get_node(&self, id: NodeId) -> Option<&ConversationNode> {
        self.nodes.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_retrieve_nodes() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "Hello", 5);
        let b = g.add_node("assistant", "Hi there", 10);
        assert_ne!(a, b);
        assert_eq!(g.get_node(a).unwrap().tokens, 5);
        assert_eq!(g.get_node(b).unwrap().role, "assistant");
    }

    #[test]
    fn add_edge_success() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let b = g.add_node("assistant", "B", 2);
        assert!(g.add_edge(a, b).is_ok());
    }

    #[test]
    fn add_edge_self_loop_rejected() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        assert_eq!(g.add_edge(a, a), Err(GraphError::InvalidEdge("self-loop".into())));
    }

    #[test]
    fn add_edge_cycle_rejected() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let b = g.add_node("assistant", "B", 2);
        g.add_edge(a, b).unwrap();
        assert_eq!(g.add_edge(b, a), Err(GraphError::CycleDetected));
    }

    #[test]
    fn add_edge_longer_cycle_rejected() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let b = g.add_node("assistant", "B", 2);
        let c = g.add_node("user", "C", 3);
        g.add_edge(a, b).unwrap();
        g.add_edge(b, c).unwrap();
        assert_eq!(g.add_edge(c, a), Err(GraphError::CycleDetected));
    }

    #[test]
    fn add_edge_unknown_node() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let phantom = NodeId(999);
        assert_eq!(g.add_edge(a, phantom), Err(GraphError::NodeNotFound(phantom)));
        assert_eq!(g.add_edge(phantom, a), Err(GraphError::NodeNotFound(phantom)));
    }

    #[test]
    fn branch_from_creates_child() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "Hello", 5);
        let b = g.branch_from(a).unwrap();
        assert_ne!(a, b);
        // The clone should have the same content
        assert_eq!(g.get_node(b).unwrap().content, "Hello");
    }

    #[test]
    fn merge_paths_creates_merge_node() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let b = g.add_node("assistant", "B", 2);
        let c = g.add_node("user", "C", 3);
        let merge = g.merge_paths(&[a], &[b, c]).unwrap();
        let merge_node = g.get_node(merge).unwrap();
        assert_eq!(merge_node.role, "merge");
    }

    #[test]
    fn linearize_simple_chain() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 1);
        let b = g.add_node("assistant", "B", 2);
        let c = g.add_node("user", "C", 3);
        g.add_edge(a, b).unwrap();
        g.add_edge(b, c).unwrap();
        let order = g.linearize(a).unwrap();
        assert_eq!(order, vec![a, b, c]);
    }

    #[test]
    fn linearize_diamond() {
        let mut g = ConversationGraph::new();
        let root = g.add_node("system", "Root", 0);
        let left = g.add_node("user", "Left", 1);
        let right = g.add_node("user", "Right", 2);
        let merge = g.add_node("assistant", "Merge", 3);
        g.add_edge(root, left).unwrap();
        g.add_edge(root, right).unwrap();
        g.add_edge(left, merge).unwrap();
        g.add_edge(right, merge).unwrap();
        let order = g.linearize(root).unwrap();
        assert_eq!(order.len(), 4);
        // root must come first, merge must come last
        assert_eq!(order[0], root);
        assert_eq!(order[3], merge);
    }

    #[test]
    fn path_tokens_sums_correctly() {
        let mut g = ConversationGraph::new();
        let a = g.add_node("user", "A", 10);
        let b = g.add_node("assistant", "B", 20);
        let c = g.add_node("user", "C", 5);
        assert_eq!(g.path_tokens(&[a, b, c]), 35);
    }

    #[test]
    fn linearize_unknown_root() {
        let g = ConversationGraph::new();
        assert_eq!(
            g.linearize(NodeId(42)),
            Err(GraphError::NodeNotFound(NodeId(42)))
        );
    }
}
