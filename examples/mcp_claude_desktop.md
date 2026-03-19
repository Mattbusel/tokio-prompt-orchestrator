# MCP Server Setup for Claude Desktop and Claude Code

This guide explains how to build and configure the `tokio-prompt-orchestrator` MCP
(Model Context Protocol) server so that Claude Desktop and Claude Code can call it as
a tool.

---

## 1. Build the MCP binary

```bash
cargo build --release --features mcp --bin mcp
```

The resulting binary is at `target/release/mcp` (Linux/macOS) or
`target\release\mcp.exe` (Windows).

---

## 2. Add to Claude Desktop

Edit (or create) the Claude Desktop configuration file:

- **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`
- **Linux**: `~/.config/Claude/claude_desktop_config.json`

```json
{
  "mcpServers": {
    "tokio-prompt-orchestrator": {
      "command": "/absolute/path/to/target/release/mcp",
      "args": [],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

Replace `/absolute/path/to/target/release/mcp` with the actual path on your machine.

Restart Claude Desktop after saving.  You should see the orchestrator tools listed
in the tool picker (the hammer icon).

---

## 3. Add to Claude Code

Edit (or create) `.claude/settings.json` in your project root (or in
`~/.claude/settings.json` for a global install):

```json
{
  "mcpServers": {
    "tokio-prompt-orchestrator": {
      "command": "/absolute/path/to/target/release/mcp",
      "args": [],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

Or, using Claude Code's CLI:

```bash
claude mcp add tokio-prompt-orchestrator /absolute/path/to/target/release/mcp
```

---

## 4. Available MCP tools

Once connected, Claude can call the following tools:

| Tool | Description |
|------|-------------|
| `infer` | Submit a single prompt to the pipeline and return the response. |
| `batch_infer` | Submit multiple prompts in one call; responses are returned in order. |
| `pipeline_status` | Return live metrics: queue depths, circuit breaker state, dedup cache hits, error counts. |
| `configure_pipeline` | Update pipeline configuration at runtime (hot-reload). Accepts a TOML string. |

### Tool schemas (simplified)

```
infer(prompt: string, session_id?: string) -> string
batch_infer(prompts: string[], session_id?: string) -> string[]
pipeline_status() -> { queue_depths, circuit_breaker, dedup_stats, error_counts }
configure_pipeline(toml: string) -> { ok: bool, error?: string }
```

---

## 5. Example prompts to use with Claude

Once the MCP server is running and Claude Desktop / Claude Code is configured, you
can use natural language to drive the orchestrator:

### Basic inference

> "Use the orchestrator to answer: What are the key principles of Rust's ownership model?"

### Batch inference

> "Send these three prompts to the orchestrator pipeline and show me all three responses:
> 1. Explain async/await in Rust
> 2. What is a Tokio runtime?
> 3. How does backpressure work in channel-based systems?"

### Pipeline health check

> "Check the current status of the prompt orchestrator pipeline."

### Hot config update

> "Update the orchestrator pipeline config to use 5 retry attempts instead of 3.
> Here is the relevant TOML snippet: `[resilience]\nretry_attempts = 5`"

### Dedup savings

> "Ask the orchestrator the same question five times simultaneously and tell me
> how many were deduplicated vs actually processed."

---

## 6. Running the MCP server manually (for testing)

```bash
# Start the MCP server on stdin/stdout (the transport Claude Desktop uses)
./target/release/mcp

# With custom log level
RUST_LOG=debug ./target/release/mcp
```

The server communicates over stdin/stdout using the JSON-RPC MCP protocol.
You can also use `mcp dev` from the MCP CLI toolkit to inspect tool calls
interactively.

---

## 7. Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Log verbosity (`error`, `warn`, `info`, `debug`, `trace`) |
| `RUST_LOG_FORMAT` | `pretty` | Set to `json` for structured JSON logs |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | (none) | Send traces to an OpenTelemetry collector |
| `JAEGER_ENDPOINT` | (none) | Shorthand for Jaeger HTTP endpoint |

---

## 8. Security notes

- The MCP binary listens on stdin/stdout only; it does not open any network port.
- No API keys are required for the default `echo` worker.  Real deployments using
  OpenAI or Anthropic workers require `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.
- Never commit API keys to version control.  Use environment variables or a secrets
  manager.
