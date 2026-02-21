//! Integration tests for the MCP server module.
//!
//! These tests verify the MCP tool implementations end-to-end: infer tool
//! response shapes, pipeline status reporting, batch inference submission,
//! and configuration validation.

#[cfg(feature = "mcp")]
mod tool_batch;
#[cfg(feature = "mcp")]
mod tool_configure;
#[cfg(feature = "mcp")]
mod tool_infer;
#[cfg(feature = "mcp")]
mod tool_status;
