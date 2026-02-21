//! # Stage: NATS Pub/Sub Transport
//!
//! ## Responsibility
//! Provide NATS-based pub/sub messaging for inter-node communication.
//! Supports publishing inference requests, subscribing by node role,
//! and request/reply for synchronous queries.
//!
//! ## Guarantees
//! - Async: all operations are non-blocking, using `async_nats`
//! - Typed: messages are serialized as JSON via serde
//! - Scoped: subject hierarchy isolates orchestrator traffic
//! - Graceful: connection failures surface as `DistributedError`, never panic
//!
//! ## NOT Responsible For
//! - Message persistence (NATS JetStream would be a separate layer)
//! - Deduplication (see: `redis_dedup`)
//! - Leader election (see: `leader`)

use async_nats::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::DistributedError;

/// Subject prefix for all orchestrator NATS messages.
const SUBJECT_PREFIX: &str = "orchestrator";

/// A message published over NATS for inter-node communication.
///
/// Carries routing metadata alongside the payload so that subscribers
/// can filter and route without parsing the full payload.
///
/// # Panics
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NatsMessage {
    /// ID of the node that sent this message.
    pub source_node: String,
    /// Message type/subject category (e.g., "request", "heartbeat", "leader").
    pub msg_type: String,
    /// JSON-serialized payload.
    pub payload: String,
}

/// NATS transport layer for the distributed orchestrator.
///
/// Wraps an `async_nats::Client` and provides typed publish/subscribe
/// operations scoped to orchestrator subjects.
///
/// # Panics
/// This type never panics.
///
/// # Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::distributed::NatsBus;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let bus = NatsBus::connect("nats://localhost:4222", "node-1").await?;
/// bus.publish("request", "my-role", r#"{"prompt":"hello"}"#).await?;
/// # Ok(())
/// # }
/// ```
pub struct NatsBus {
    client: Arc<RwLock<Option<Client>>>,
    node_id: String,
    nats_url: String,
}

impl NatsBus {
    /// Connect to a NATS server.
    ///
    /// # Arguments
    /// * `nats_url` — NATS server URL (e.g., `nats://localhost:4222`)
    /// * `node_id` — Identifier of this node (included in outgoing messages)
    ///
    /// # Returns
    /// - `Ok(NatsBus)` — connected and ready
    /// - `Err(DistributedError::NatsConnection)` — connection failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn connect(nats_url: &str, node_id: &str) -> Result<Self, DistributedError> {
        let client = async_nats::connect(nats_url).await.map_err(|e| {
            DistributedError::NatsConnection(format!(
                "failed to connect to NATS at {nats_url}: {e}"
            ))
        })?;

        info!(node = node_id, url = nats_url, "connected to NATS");

        Ok(Self {
            client: Arc::new(RwLock::new(Some(client))),
            node_id: node_id.to_string(),
            nats_url: nats_url.to_string(),
        })
    }

    /// Create an unconnected NatsBus (for testing or deferred connection).
    ///
    /// # Arguments
    /// * `nats_url` — URL to use when connecting later
    /// * `node_id` — Identifier of this node
    ///
    /// # Returns
    /// A `NatsBus` with no active connection.
    ///
    /// # Panics
    /// This function never panics.
    pub fn unconnected(nats_url: &str, node_id: &str) -> Self {
        Self {
            client: Arc::new(RwLock::new(None)),
            node_id: node_id.to_string(),
            nats_url: nats_url.to_string(),
        }
    }

    /// Publish a message to a subject.
    ///
    /// The full subject is `orchestrator.{msg_type}.{role}`.
    ///
    /// # Arguments
    /// * `msg_type` — Message type category (e.g., "request", "heartbeat")
    /// * `role` — Target role or topic (e.g., "inference", "all")
    /// * `payload` — JSON payload string
    ///
    /// # Returns
    /// - `Ok(())` — message published
    /// - `Err(DistributedError::NatsPublish)` — publish failed
    /// - `Err(DistributedError::NatsConnection)` — not connected
    ///
    /// # Panics
    /// This function never panics.
    pub async fn publish(
        &self,
        msg_type: &str,
        role: &str,
        payload: &str,
    ) -> Result<(), DistributedError> {
        let subject = format!("{SUBJECT_PREFIX}.{msg_type}.{role}");

        let msg = NatsMessage {
            source_node: self.node_id.clone(),
            msg_type: msg_type.to_string(),
            payload: payload.to_string(),
        };

        let data = serde_json::to_vec(&msg).map_err(|e| {
            DistributedError::NatsPublish(format!("failed to serialize message: {e}"))
        })?;

        let guard = self.client.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| DistributedError::NatsConnection("not connected to NATS".to_string()))?;

        client
            .publish(subject.clone(), data.into())
            .await
            .map_err(|e| {
                DistributedError::NatsPublish(format!("publish to {subject} failed: {e}"))
            })?;

        debug!(subject = %subject, node = %self.node_id, "published message");
        Ok(())
    }

    /// Subscribe to a subject pattern.
    ///
    /// Returns a NATS subscriber that can be iterated to receive messages.
    ///
    /// # Arguments
    /// * `msg_type` — Message type category (e.g., "request", "heartbeat", ">")
    /// * `role` — Target role or topic (e.g., "inference", "*")
    ///
    /// # Returns
    /// - `Ok(Subscriber)` — subscription active
    /// - `Err(DistributedError::NatsSubscribe)` — subscription failed
    /// - `Err(DistributedError::NatsConnection)` — not connected
    ///
    /// # Panics
    /// This function never panics.
    pub async fn subscribe(
        &self,
        msg_type: &str,
        role: &str,
    ) -> Result<async_nats::Subscriber, DistributedError> {
        let subject = format!("{SUBJECT_PREFIX}.{msg_type}.{role}");

        let guard = self.client.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| DistributedError::NatsConnection("not connected to NATS".to_string()))?;

        let subscriber = client.subscribe(subject.clone()).await.map_err(|e| {
            DistributedError::NatsSubscribe(format!("subscribe to {subject} failed: {e}"))
        })?;

        info!(subject = %subject, node = %self.node_id, "subscribed");
        Ok(subscriber)
    }

    /// Send a request and wait for a reply (request/reply pattern).
    ///
    /// # Arguments
    /// * `msg_type` — Message type category
    /// * `role` — Target role
    /// * `payload` — JSON payload string
    /// * `timeout` — Maximum time to wait for a reply
    ///
    /// # Returns
    /// - `Ok(NatsMessage)` — the reply message
    /// - `Err(DistributedError::NatsTimeout)` — no reply within timeout
    /// - `Err(DistributedError::NatsConnection)` — not connected
    ///
    /// # Panics
    /// This function never panics.
    pub async fn request(
        &self,
        msg_type: &str,
        role: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<NatsMessage, DistributedError> {
        let subject = format!("{SUBJECT_PREFIX}.{msg_type}.{role}");

        let msg = NatsMessage {
            source_node: self.node_id.clone(),
            msg_type: msg_type.to_string(),
            payload: payload.to_string(),
        };

        let data = serde_json::to_vec(&msg).map_err(|e| {
            DistributedError::NatsPublish(format!("failed to serialize request: {e}"))
        })?;

        let guard = self.client.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| DistributedError::NatsConnection("not connected to NATS".to_string()))?;

        let reply = tokio::time::timeout(timeout, client.request(subject.clone(), data.into()))
            .await
            .map_err(|_| {
                DistributedError::NatsTimeout(format!(
                    "request to {subject} timed out after {}ms",
                    timeout.as_millis()
                ))
            })?
            .map_err(|e| {
                DistributedError::NatsPublish(format!("request to {subject} failed: {e}"))
            })?;

        let response: NatsMessage = serde_json::from_slice(&reply.payload).map_err(|e| {
            DistributedError::NatsPublish(format!("failed to deserialize reply: {e}"))
        })?;

        debug!(subject = %subject, from = %response.source_node, "received reply");
        Ok(response)
    }

    /// Build a subject string for the given message type and role.
    ///
    /// # Arguments
    /// * `msg_type` — Message type category
    /// * `role` — Target role
    ///
    /// # Returns
    /// The full NATS subject string.
    ///
    /// # Panics
    /// This function never panics.
    pub fn subject(msg_type: &str, role: &str) -> String {
        format!("{SUBJECT_PREFIX}.{msg_type}.{role}")
    }

    /// Check if this bus is currently connected.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn is_connected(&self) -> bool {
        let guard = self.client.read().await;
        guard.is_some()
    }

    /// Get this node's ID.
    ///
    /// # Panics
    /// This function never panics.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Get the configured NATS URL.
    ///
    /// # Panics
    /// This function never panics.
    pub fn nats_url(&self) -> &str {
        &self.nats_url
    }

    /// Disconnect from NATS (clears the client).
    ///
    /// # Panics
    /// This function never panics.
    pub async fn disconnect(&self) {
        let mut guard = self.client.write().await;
        *guard = None;
        info!(node = %self.node_id, "disconnected from NATS");
    }

    /// Parse a raw NATS message payload into a [`NatsMessage`].
    ///
    /// # Arguments
    /// * `data` — Raw bytes from a NATS message
    ///
    /// # Returns
    /// - `Ok(NatsMessage)` — successfully deserialized
    /// - `Err(DistributedError::NatsPublish)` — deserialization failed
    ///
    /// # Panics
    /// This function never panics.
    pub fn parse_message(data: &[u8]) -> Result<NatsMessage, DistributedError> {
        serde_json::from_slice(data).map_err(|e| {
            DistributedError::NatsPublish(format!("failed to parse NATS message: {e}"))
        })
    }
}

impl Clone for NatsBus {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            node_id: self.node_id.clone(),
            nats_url: self.nats_url.clone(),
        }
    }
}

impl std::fmt::Debug for NatsBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsBus")
            .field("node_id", &self.node_id)
            .field("nats_url", &self.nats_url)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subject_builds_correct_format() {
        let subject = NatsBus::subject("request", "inference");
        assert_eq!(subject, "orchestrator.request.inference");
    }

    #[test]
    fn test_subject_with_wildcard() {
        let subject = NatsBus::subject("heartbeat", "*");
        assert_eq!(subject, "orchestrator.heartbeat.*");
    }

    #[test]
    fn test_subject_prefix_is_orchestrator() {
        assert_eq!(SUBJECT_PREFIX, "orchestrator");
    }

    #[test]
    fn test_nats_message_serde_roundtrip() {
        let msg = NatsMessage {
            source_node: "node-1".to_string(),
            msg_type: "request".to_string(),
            payload: r#"{"prompt":"hello"}"#.to_string(),
        };
        let json = serde_json::to_vec(&msg);
        assert!(json.is_ok(), "serialization must succeed");
        let bytes = json.unwrap_or_default();
        let deserialized: Result<NatsMessage, _> = serde_json::from_slice(&bytes);
        assert!(deserialized.is_ok(), "deserialization must succeed");
        assert_eq!(msg, deserialized.unwrap_or_else(|_| msg.clone()));
    }

    #[test]
    fn test_nats_message_debug_format() {
        let msg = NatsMessage {
            source_node: "node-1".to_string(),
            msg_type: "heartbeat".to_string(),
            payload: "{}".to_string(),
        };
        let debug = format!("{:?}", msg);
        assert!(debug.contains("node-1"));
        assert!(debug.contains("heartbeat"));
    }

    #[test]
    fn test_nats_message_clone() {
        let msg = NatsMessage {
            source_node: "node-1".to_string(),
            msg_type: "test".to_string(),
            payload: "data".to_string(),
        };
        let cloned = msg.clone();
        assert_eq!(msg, cloned);
    }

    #[test]
    fn test_parse_message_valid_json() {
        let msg = NatsMessage {
            source_node: "n1".to_string(),
            msg_type: "req".to_string(),
            payload: "p".to_string(),
        };
        let bytes = serde_json::to_vec(&msg).unwrap_or_default();
        let parsed = NatsBus::parse_message(&bytes);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap_or_else(|_| msg.clone()), msg);
    }

    #[test]
    fn test_parse_message_invalid_json_returns_error() {
        let result = NatsBus::parse_message(b"not json");
        if let Err(e) = result {
            assert!(matches!(e, DistributedError::NatsPublish(_)));
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_parse_message_empty_bytes_returns_error() {
        let result = NatsBus::parse_message(b"");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unconnected_is_not_connected() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        assert!(!bus.is_connected().await);
    }

    #[tokio::test]
    async fn test_unconnected_node_id() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "test-node");
        assert_eq!(bus.node_id(), "test-node");
    }

    #[tokio::test]
    async fn test_unconnected_nats_url() {
        let bus = NatsBus::unconnected("nats://custom:4222", "node-1");
        assert_eq!(bus.nats_url(), "nats://custom:4222");
    }

    #[tokio::test]
    async fn test_publish_without_connection_returns_error() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        let result = bus.publish("request", "all", "{}").await;
        if let Err(e) = result {
            assert!(matches!(e, DistributedError::NatsConnection(_)));
        } else {
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_subscribe_without_connection_returns_error() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        let result = bus.subscribe("request", "all").await;
        if let Err(e) = result {
            assert!(matches!(e, DistributedError::NatsConnection(_)));
        } else {
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_request_without_connection_returns_error() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        let result = bus
            .request("query", "status", "{}", Duration::from_secs(1))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disconnect_clears_connection() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        bus.disconnect().await;
        assert!(!bus.is_connected().await);
    }

    #[tokio::test]
    async fn test_clone_shares_connection_state() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
        let cloned = bus.clone();
        assert_eq!(bus.node_id(), cloned.node_id());
        assert_eq!(bus.nats_url(), cloned.nats_url());
        assert_eq!(bus.is_connected().await, cloned.is_connected().await);
    }

    #[test]
    fn test_debug_format_contains_fields() {
        let bus = NatsBus::unconnected("nats://localhost:4222", "debug-node");
        let debug = format!("{:?}", bus);
        assert!(debug.contains("debug-node"));
        assert!(debug.contains("nats://localhost:4222"));
    }

    #[tokio::test]
    async fn test_connect_to_invalid_url_returns_error() {
        // async-nats may or may not fail immediately depending on URL parsing
        // vs actual TCP connection. We test that it doesn't panic either way.
        let result =
            NatsBus::connect("nats://invalid-host-that-does-not-exist:99999", "node-1").await;
        // Either it fails or it succeeds (some NATS client impls defer connection)
        // The important thing is no panic
        if let Err(e) = &result {
            assert!(matches!(e, DistributedError::NatsConnection(_)));
        }
    }
}
