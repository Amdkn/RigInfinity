//! Agent loop — native port of the agent-zero engine loop
//! (D-N051 F8, pas 5, D4a).
//!
//! Upstream (`agent0ai/agent-zero`, gel `ba9b67d`), `agent.py:364`
//! `Agent.monologue`:
//!   message -> model -> tools -> memory -> response
//!
//! The native port is a re-implementation of the *function*, not a
//! line-by-line copy: a state machine (`AgentLoop`) driven by a model
//! closure and a tool table. The rules carried over from the upstream
//! DOX (`agent-zero-repo/AGENTS.md`):
//! - Message-loop completion requires an explicit response-tool call
//!   with `break_loop`. Free text in either transport remains
//!   generation thoughts and silently continues; never synthesize a
//!   response-tool call from prose.
//! - Malformed tool intent keeps its repair path (returned as a
//!   warning in the history, not raised out of the turn).
//! - `hist_add_ai_response` owns Responses-API state advancement: it
//!   calls the internal state-remembering step itself. Callers must
//!   not invoke it manually.
//! - `call_chat_model_turn` builds native function tools only when the
//!   chat model's `a0_api_mode` selects Responses; Chat Completions
//!   turns pass an empty tool list.
//!
//! NOT ported: the WebUI hooks, the MCP server start, the prompt
//! template machinery (`prompts/`), the extension hooks. The loop is
//! the engine; the surfaces re-point at the daemon.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// One tool handler: (args) -> result text or error.
pub type ToolHandler = Box<dyn Fn(&serde_json::Value) -> Result<String, BoxError>>;
/// The tool table a turn dispatches into.
pub type ToolTable = HashMap<String, ToolHandler>;

/// Iteration guard — the loop never runs unbounded.
pub const MAX_ITERATIONS: u32 = 25;
/// The response tool that completes a message loop.
pub const BREAK_LOOP_TOOL: &str = "break_loop";

/// Loop state — mirror of the upstream agent state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    Idle,
    Thinking,
    ToolCall,
    Responding,
    Done,
    Intervention,
}

/// One history entry — mirror of `Agent.hist_add_*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub role: String, // user | assistant | tool
    pub content: String,
    #[serde(default)]
    pub kind: String, // message | ai_response | tool_result | warning
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_id: String,
}

/// A chat message as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// A tool request extracted from model text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequest {
    pub tool: String,
    pub args: serde_json::Value,
}

/// Extract a tool request from model text.
///
/// Wire format (adapter contract): a fenced JSON block carrying
/// `{"tool": <name>, "args": {...}}`. Returns `None` when no such
/// block is present — free text is generation thoughts, never a
/// synthesized tool call. A dict carrying a `tool` key is returned
/// even when malformed: validation owns the repair path.
pub fn extract_tool_request(text: &str) -> Option<ToolRequest> {
    let fence = text.find("```json").or_else(|| text.find("```"))?;
    let body_start = text[fence..].find('\n')? + fence + 1;
    let end = text[body_start..].find("```")? + body_start;
    let parsed: serde_json::Value = serde_json::from_str(text[body_start..end].trim()).ok()?;
    let obj = parsed.as_object()?;
    let tool = obj.get("tool")?;
    Some(ToolRequest {
        tool: match tool.as_str() {
            Some(s) => s.to_owned(),
            None => String::new(), // malformed: validation rejects it
        },
        args: obj.get("args").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// Validate a tool request; malformed intent keeps its repair path.
///
/// Returns `Ok(())` or the repair message the loop feeds back to
/// the model.
pub fn validate_tool_request(request: &ToolRequest) -> Result<(), String> {
    if request.tool.is_empty() {
        return Err("Tool request is missing a string 'tool' name.".to_owned());
    }
    if !request.args.is_null() && !request.args.is_object() {
        return Err("Tool request 'args' must be a JSON object.".to_owned());
    }
    Ok(())
}

/// The Agent loop, native. Mirrors `Agent` (`agent.py:364`).
///
/// `model` is a closure `(messages) -> String`; `tools` maps tool
/// names to closures `(args) -> Result<String, BoxError>`. The loop
/// never calls the fork: `agent.py` and `models.py` are absent from
/// the execution path (H-P5-2).
pub struct AgentLoop {
    pub number: u32,
    pub agent_name: String,
    pub config: HashMap<String, String>,
    pub history: Vec<HistoryEntry>,
    pub data: HashMap<String, serde_json::Value>,
    pub state: LoopState,
    pub last_user_message: Option<String>,
    pub iteration: u32,
}

impl AgentLoop {
    pub fn new(number: u32) -> Self {
        Self {
            number,
            agent_name: format!("A{number}"),
            config: HashMap::new(),
            history: Vec::new(),
            data: HashMap::new(),
            state: LoopState::Idle,
            last_user_message: None,
            iteration: 0,
        }
    }

    // -- history (mirror of Agent.hist_add_*; agent.py:725-830) --------

    pub fn hist_add_message(&mut self, role: &str, content: &str) -> HistoryEntry {
        let entry = HistoryEntry {
            role: role.to_owned(),
            content: content.to_owned(),
            kind: "message".to_owned(),
            tool_name: String::new(),
            response_id: String::new(),
        };
        self.history.push(entry.clone());
        entry
    }

    pub fn hist_add_user_message(&mut self, message: &str, intervention: bool) -> HistoryEntry {
        self.last_user_message = Some(message.to_owned());
        if intervention {
            self.state = LoopState::Intervention;
        }
        self.hist_add_message("user", message)
    }

    /// Owns Responses-API state advancement (upstream rule).
    ///
    /// When the model turn passes an LLM result carrying a response
    /// id, the state is remembered here, internally. Callers must not
    /// invoke the remembering step manually.
    pub fn hist_add_ai_response(
        &mut self,
        content: &str,
        response_id: Option<&str>,
    ) -> HistoryEntry {
        let entry = HistoryEntry {
            role: "assistant".to_owned(),
            content: content.to_owned(),
            kind: "ai_response".to_owned(),
            tool_name: String::new(),
            response_id: response_id.unwrap_or("").to_owned(),
        };
        self.history.push(entry.clone());
        if let Some(id) = response_id {
            self.remember_llm_result_state(id);
        }
        entry
    }

    fn remember_llm_result_state(&mut self, response_id: &str) {
        self.data.insert(
            "responses_state".to_owned(),
            serde_json::json!({
                "response_id": response_id,
                "history_counter": self.history.len(),
            }),
        );
    }

    pub fn hist_add_warning(&mut self, message: &str) -> HistoryEntry {
        let entry = HistoryEntry {
            role: "assistant".to_owned(),
            content: message.to_owned(),
            kind: "warning".to_owned(),
            tool_name: String::new(),
            response_id: String::new(),
        };
        self.history.push(entry.clone());
        entry
    }

    pub fn hist_add_tool_result(&mut self, tool_name: &str, tool_result: &str) -> HistoryEntry {
        let entry = HistoryEntry {
            role: "tool".to_owned(),
            content: tool_result.to_owned(),
            kind: "tool_result".to_owned(),
            tool_name: tool_name.to_owned(),
            response_id: String::new(),
        };
        self.history.push(entry.clone());
        entry
    }

    // -- prompt ---------------------------------------------------------

    pub fn prepare_prompt(&self) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage {
            role: "system".to_owned(),
            content: self
                .config
                .get("system_prompt")
                .cloned()
                .unwrap_or_else(|| "You are A0, the native agent engine.".to_owned()),
        }];
        for entry in &self.history {
            messages.push(ChatMessage {
                role: entry.role.clone(),
                content: entry.content.clone(),
            });
        }
        messages
    }

    // -- the loop -------------------------------------------------------

    /// One user turn: message -> model -> tools -> memory -> response.
    ///
    /// Returns the final assistant response text. The loop completes
    /// only on an explicit response-tool call carrying `break_loop`.
    pub fn turn(
        &mut self,
        user_message: &str,
        model: &mut dyn FnMut(&[ChatMessage]) -> String,
        tools: &ToolTable,
    ) -> String {
        self.hist_add_user_message(user_message, false);
        self.iteration = 0;
        self.state = LoopState::Thinking;
        let mut final_response = String::new();

        while self.iteration < MAX_ITERATIONS {
            self.iteration += 1;
            let prompt = self.prepare_prompt();
            let text = model(&prompt);
            let Some(request) = extract_tool_request(&text) else {
                // Free text is generation thoughts; silently continue.
                self.state = LoopState::Thinking;
                continue;
            };

            if let Err(repair) = validate_tool_request(&request) {
                // Malformed tool intent keeps its repair path.
                self.hist_add_warning(&repair);
                self.state = LoopState::Thinking;
                continue;
            }

            if request.tool == BREAK_LOOP_TOOL {
                final_response = request
                    .args
                    .get("response")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                self.hist_add_ai_response(&final_response, None);
                self.state = LoopState::Done;
                return final_response;
            }

            self.state = LoopState::ToolCall;
            let result = match tools.get(&request.tool) {
                None => format!("error: unknown tool '{}'", request.tool),
                Some(handler) => match handler(&request.args) {
                    Ok(out) => out,
                    Err(e) => format!("error: {e}"),
                },
            };
            self.hist_add_tool_result(&request.tool, &result);
            self.state = LoopState::Thinking;
        }

        // Iteration guard reached without an explicit break_loop.
        self.hist_add_warning(&format!(
            "loop guard: {MAX_ITERATIONS} iterations without break_loop"
        ));
        self.state = LoopState::Done;
        final_response
    }

    /// Chat persistence shape: history + state, no fork involved.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "agent": self.agent_name,
            "state": format!("{:?}", self.state).to_lowercase(),
            "iterations": self.iteration,
            "history": self.history,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_tool_then_break() -> impl FnMut(&[ChatMessage]) -> String {
        let mut n = 0;
        move |_messages: &[ChatMessage]| {
            n += 1;
            if n == 1 {
                "```json\n{\"tool\": \"echo\", \"args\": {\"text\": \"hello\"}}\n```".to_owned()
            } else {
                "```json\n{\"tool\": \"break_loop\", \"args\": {\"response\": \"done: hello\"}}\n```"
                    .to_owned()
            }
        }
    }

    #[test]
    fn loop_tool_then_break() {
        let mut agent = AgentLoop::new(0);
        let mut tools: ToolTable = HashMap::new();
        tools.insert(
            "echo".to_owned(),
            Box::new(|args| {
                Ok(args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned())
            }),
        );
        let result = agent.turn("say hello", &mut model_tool_then_break(), &tools);
        assert_eq!(result, "done: hello");
        assert_eq!(agent.state, LoopState::Done);
        assert!(agent.history.iter().any(|e| e.kind == "tool_result"));
        assert_eq!(
            agent
                .history
                .last()
                .expect("history holds the ai_response")
                .kind,
            "ai_response"
        );
    }

    #[test]
    fn free_text_never_completes() {
        let mut agent = AgentLoop::new(0);
        let tools = HashMap::new();
        let result = agent.turn(
            "talk",
            &mut |_| "just some prose, no tool".to_owned(),
            &tools,
        );
        assert_eq!(result, "");
        assert_eq!(agent.state, LoopState::Done);
        assert_eq!(agent.iteration, MAX_ITERATIONS);
    }

    #[test]
    fn malformed_tool_keeps_repair_path() {
        let mut agent = AgentLoop::new(0);
        let tools = HashMap::new();
        let mut n = 0;
        let result = agent.turn(
            "fix me",
            &mut move |_| {
                n += 1;
                if n == 1 {
                    "```json\n{\"tool\": 42}\n```".to_owned()
                } else {
                    "```json\n{\"tool\": \"break_loop\", \"args\": {\"response\": \"repaired\"}}\n```"
                        .to_owned()
                }
            },
            &tools,
        );
        assert_eq!(result, "repaired");
        assert!(agent.history.iter().any(|e| e.kind == "warning"));
    }

    #[test]
    fn ai_response_owns_state_advancement() {
        let mut agent = AgentLoop::new(0);
        let entry = agent.hist_add_ai_response("hi", Some("resp_123"));
        assert_eq!(entry.response_id, "resp_123");
        let state = &agent.data["responses_state"];
        assert_eq!(state["response_id"], "resp_123");
        assert_eq!(state["history_counter"], 1);
    }

    #[test]
    fn snapshot_is_fork_free() {
        let mut agent = AgentLoop::new(0);
        agent.hist_add_user_message("hi", false);
        let snap = agent.snapshot();
        assert_eq!(snap["agent"], "A0");
        let dumped = snap.to_string();
        assert!(!dumped.contains("agent.py"));
        assert!(!dumped.contains("models.py"));
    }
}
