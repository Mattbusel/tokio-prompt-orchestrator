# Distributed Deployment Guide

This guide covers deploying `tokio-prompt-orchestrator` across multiple nodes
using Redis for cross-node deduplication and NATS for coordination.

---

## 1. Architecture Overview

### Single-Node

A single process handles all pipeline stages. Suitable for development and
low-traffic deployments.

### Multi-Node

Each node runs an identical binary. Redis provides cross-node deduplication
so that two nodes receiving the same prompt within the TTL window share a
single inference result. NATS is used for leader election and distributing
work items across the fleet.

---

## 2. Prerequisites

### Redis

Redis 6.2 or later.

```
docker run -d --name redis -p 6379:6379 redis:7-alpine
```

### NATS

NATS Server 2.10 or later with JetStream enabled.

```
docker run -d --name nats -p 4222:4222 nats:2.10-alpine -js
```

---

## 3. Environment Variables

| Variable | Required | Description |
|---|---|---|
| REDIS_URL | Yes (distributed) | Redis connection URL, e.g. redis://redis:6379 |
| NATS_URL | Yes (distributed) | NATS server URL, e.g. nats://nats:4222 |
| API_KEY | Recommended | Bearer token for the REST/WebSocket API. |
| METRICS_API_KEY | Recommended | Separate key for the /metrics scrape endpoint. |
| ALLOWED_ORIGINS | Recommended | Comma-separated CORS origins for the web API. |
| RUST_LOG | Optional | Tracing filter (default: info). |
| RUST_LOG_FORMAT | Optional | Set to json for structured logs. |

---

## 4. Feature Flags

The `distributed` feature must be enabled at compile time:

```toml
[features]
distributed = ["redis", "async-nats"]
```

Build command:

```
cargo build --release --features distributed,web-api,metrics-server
```

---

## 5. Session Affinity via `shard_session()`

When running multiple nodes behind a load balancer, route each session to a
consistent node using the `shard_session` helper:

```rust
use tokio_prompt_orchestrator::{SessionId, shard_session};

let session = SessionId::new("user-abc-123");
let node_index = shard_session(&session, 3); // 0, 1, or 2
// Route to upstream[node_index]
```

`shard_session` uses FNV-1a hashing, which is deterministic across process
restarts, so the same session always maps to the same shard after a redeploy.

---

## 6. Sample Kubernetes Deployment (3 replicas)

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: tokio-prompt-orchestrator
  labels:
    app: orchestrator
spec:
  replicas: 3
  selector:
    matchLabels:
      app: orchestrator
  template:
    metadata:
      labels:
        app: orchestrator
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "9090"
        prometheus.io/path: "/metrics"
    spec:
      terminationGracePeriodSeconds: 60
      containers:
        - name: orchestrator
          image: your-registry/tokio-prompt-orchestrator:latest
          ports:
            - containerPort: 8080
              name: http
            - containerPort: 9090
              name: metrics
          env:
            - name: REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: orchestrator-secrets
                  key: redis-url
            - name: NATS_URL
              valueFrom:
                secretKeyRef:
                  name: orchestrator-secrets
                  key: nats-url
            - name: API_KEY
              valueFrom:
                secretKeyRef:
                  name: orchestrator-secrets
                  key: api-key
            - name: METRICS_API_KEY
              valueFrom:
                secretKeyRef:
                  name: orchestrator-secrets
                  key: metrics-api-key
            - name: RUST_LOG_FORMAT
              value: json
          readinessProbe:
            httpGet:
              path: /health
              port: 8080
            initialDelaySeconds: 5
            periodSeconds: 10
          livenessProbe:
            httpGet:
              path: /health
              port: 8080
            initialDelaySeconds: 15
            periodSeconds: 30
          resources:
            requests:
              cpu: "500m"
              memory: "256Mi"
            limits:
              cpu: "2000m"
              memory: "1Gi"
---
apiVersion: v1
kind: Service
metadata:
  name: orchestrator
spec:
  selector:
    app: orchestrator
  ports:
    - name: http
      port: 80
      targetPort: 8080
    - name: metrics
      port: 9090
      targetPort: 9090
```

---

## 7. Prometheus Scrape Config for Multi-Pod

```yaml
scrape_configs:
  - job_name: orchestrator
    kubernetes_sd_configs:
      - role: pod
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_scrape]
        action: keep
        regex: "true"
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_port]
        action: replace
        target_label: __address__
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_path]
        action: replace
        target_label: __metrics_path__
    authorization:
      credentials: "<your-metrics-api-key>"
```

---

## 8. Graceful Shutdown

The `PipelineHandles::shutdown()` method signals all pipeline stage tasks to
drain in-flight requests and stop accepting new work.

```rust
let handles = spawn_pipeline(worker);
tokio::signal::ctrl_c().await?;
handles.shutdown(); // cancel all stage tasks
```

Pair with Kubernetes `terminationGracePeriodSeconds: 60` so in-flight
requests complete before the pod is forcibly terminated.

---

## 9. Known Limitations

- **Per-process semantic dedup**: The in-process `Deduplicator` uses a local
  `DashMap`. Enable the `distributed` feature to route dedup through Redis.

- **Circuit breaker state**: Each node maintains its own circuit breaker.
  For global circuit breaking, aggregate health signals via NATS or Redis.

- **Embedding-based semantic dedup**: `with_semantic()` is per-process only.
  Cross-node semantic dedup requires an external vector store.

- **Leader election granularity**: NATS leader election is per-cluster. For
  per-session leadership, implement a custom coordinator using `shard_session`.
