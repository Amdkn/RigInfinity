//! Reactive Agent Protocol (RAP) types — native port of the `rap-protocol`
//! shapes used by the daemon (`crates/rap-protocol`, gel `e1a3e4b`).
//!
//! Only the shapes the daemon speaks are ported; thread ids are plain
//! `String`s here (the upstream `ThreadId` newtype lives in
//! `infinity-agent-core`, which is not ported — providers are out of
//! scope, D3). Serde tags are identical to upstream, so a callback
//! produced by an upstream RAP server deserialises here unchanged.

use serde::{Deserialize, Serialize};

/// One tool invocation, as posted to a RAP toolset server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapInvocation {
    pub operation: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    pub id: String,
    pub call_id: Option<String>,
    pub callback_url: String,
    pub group_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default)]
    pub thread_ancestors: Option<Vec<String>>,
}

/// A tool result, delivered to the invocation's callback URL — even to an
/// agent that currently has no process at all (H-P3-4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapToolResult {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<RapToolResultContent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_as: Option<Vec<DisplaySegment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
}

/// Structured content of a tool result (text and/or image).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RapToolResultContent {
    Text { text: String },
    Image { data: String, media_type: String },
}

/// A segment of display content for human-facing UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "content")]
pub enum DisplaySegment {
    #[serde(rename = "text")]
    Text(String),
    #[serde(rename = "diff")]
    Diff(DiffContent),
    #[serde(rename = "image")]
    Image(ImageContent),
}

/// Content for a diff display segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffContent {
    pub path: String,
    pub patch: String,
}

/// Content for an inline image display segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    pub data: String,
    pub media_type: String,
}

/// A subscription event streamed by a long-lived tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapSubscriptionEvent {
    pub group_id: String,
    pub tool_call_id: String,
    pub text: String,
    #[serde(default)]
    pub associative: bool,
}

/// A user-choice request (an agent asking the user to pick).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapUserChoice {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub prompt: String,
    pub choices: Vec<String>,
    #[serde(default)]
    pub default: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,
}

/// An OAuth redirect for a tool that needs authorisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapOAuth {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub auth_url: String,
}

/// A view update (display-only; never enters agent history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapViewUpdate {
    pub group_id: String,
    pub view_type: String,
    pub content: serde_json::Value,
}

/// Tagged enum for all RAP callback message types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RapCallback {
    #[serde(rename = "tool_result")]
    ToolResult(RapToolResult),
    #[serde(rename = "subscription_event")]
    SubscriptionEvent(RapSubscriptionEvent),
    #[serde(rename = "oauth")]
    OAuth(RapOAuth),
    #[serde(rename = "user_choice")]
    UserChoice(RapUserChoice),
    #[serde(rename = "view_update")]
    ViewUpdate(RapViewUpdate),
}

/// A tool definition, as listed in a toolset manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// The manifest a RAP toolset server serves at
/// `/.well-known/rap-toolset`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsetManifest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub endpoint: String,
    pub tools: Vec<ToolDef>,
    #[serde(default, rename = "needsMigration")]
    pub needs_migration: bool,
}
