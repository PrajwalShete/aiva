//! Messages exchanged with the relay over the agent WebSocket.
//!
//! The agent authenticates during the HTTP upgrade (`Authorization: Bearer <key>` on
//! `wss://<server>/agent/connect`), then sends `hello`. The relay stores nothing: it asks for
//! the tool list (`list`) when an app needs it and forwards tool calls (`call`), relaying the
//! `result` back, so new tools only need an agent update.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when these messages change incompatibly.
pub const AGENT_PROTOCOL: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    Welcome { id: String, url: String },
    List { id: u64 },
    Call { id: u64, tool: String, #[serde(default)] args: Value },
    Error { message: String },
    Pong,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// `probe`: only checking the key (`aiva setup`); the relay must not let it replace the
    /// running agent.
    Hello {
        protocol: u32,
        version: &'static str,
        device: String,
        os: &'static str,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        probe: bool,
    },
    /// `timing`: microseconds spent in the tool and in the agent overall (receipt → send).
    Result { id: u64, result: Value, #[serde(skip_serializing_if = "Option::is_none")] timing: Option<Value> },
    Ping,
}
