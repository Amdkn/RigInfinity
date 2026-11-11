//! Model providers — native port of the agent-zero provider client
//! (D-N051 F8, pas 5, D4b, D9).
//!
//! Upstream (`agent0ai/agent-zero`, gel `ba9b67d`), `models.py`:
//! `get_chat_model(provider, name)` builds a LiteLLM chat wrapper;
//! `_merge_provider_defaults` injects an API key from the .env when
//! missing (`get_api_key_raw` reads `API_KEY_<SERVICE>` /
//! `<SERVICE>_API_KEY` / `<SERVICE>_API_TOKEN`).
//!
//! Native rule (D9): the gateway EGM `:31415` is the ONLY way. No
//! LiteLLM, no provider key in native code. The modelPool chain from
//! pas 2 (`02_Factory/config.json`) is the route, with a 15-minute
//! retest: when the current link fails, fall over to the next link
//! and record the switch; after the retest window, the first link is
//! tried again.
//!
//! The transport is a trait so the failover logic is provable against
//! a stub without any network (`StubTransport` in tests, a local
//! loopback HTTP stub in `tests/agent_zero_proof.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::BoxError;

/// The gateway EGM — the only model endpoint (D9).
pub const GATEWAY_URL: &str = "http://127.0.0.1:31415/v1/chat/completions";

/// The modelPool chain from pas 2 (02_Factory/config.json).
pub const MODELPOOL_CHAIN: &[&str] = &[
    "freellmapi/auto",
    "claude-sonnet-4-5",
    "qwen3.8-max",
    "gemini-3.8-flash",
    "deepseek-v4-flash",
];

/// Retest window for the first link: 15 minutes (pas 2 rule).
pub const RETEST_SECS: u64 = 900;

/// One failover switch, recorded as receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchRecord {
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// A chat turn result through the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatResult {
    pub content: String,
    pub model: String,
    pub link: usize,
}

/// The HTTP transport behind the provider. The production transport
/// posts JSON to the gateway with reqwest; tests stub it.
#[async_trait]
pub trait GatewayTransport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, BoxError>;
}

/// Production transport: reqwest, loopback only.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GatewayTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, BoxError> {
        let resp = self.client.post(url).json(&payload).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("gateway returned HTTP {status}").into());
        }
        Ok(resp.json().await?)
    }
}

/// Chat model client speaking to the EGM gateway only (H-P5-4).
///
/// There is no `api_key` field, parameter, or environment read on
/// this struct — passing a key is refused by construction (D9).
pub struct GatewayProvider {
    pub endpoint: String,
    pub chain: Vec<String>,
    pub link: usize,
    pub switches: Vec<SwitchRecord>,
    link_failed_at: Option<Instant>,
    transport: Arc<dyn GatewayTransport>,
}

impl GatewayProvider {
    pub fn new(endpoint: &str, chain: &[&str]) -> Self {
        Self::with_transport(endpoint, chain, Arc::new(ReqwestTransport::new()))
    }

    pub fn with_transport(
        endpoint: &str,
        chain: &[&str],
        transport: Arc<dyn GatewayTransport>,
    ) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            chain: chain.iter().map(|s| s.to_string()).collect(),
            link: 0,
            switches: Vec::new(),
            link_failed_at: None,
            transport,
        }
    }

    pub fn current_model(&self) -> &str {
        &self.chain[self.link]
    }

    /// Upstream sentinel, kept as a refusal: the native provider
    /// never reads a provider key.
    pub fn get_api_key(_service: &str) -> &'static str {
        "None"
    }

    fn maybe_retest_first_link(&mut self) {
        if self.link == 0 {
            return;
        }
        if let Some(failed_at) = self.link_failed_at
            && failed_at.elapsed() >= Duration::from_secs(RETEST_SECS)
        {
            self.link = 0;
            self.link_failed_at = None;
        }
    }

    /// One chat turn through the gateway, with failover.
    ///
    /// On transport failure the next link is tried and the switch is
    /// recorded; when the chain is exhausted the last error is
    /// raised. The retest window restores the first link.
    pub async fn chat(
        &mut self,
        messages: &[crate::agent::ChatMessage],
    ) -> Result<ChatResult, BoxError> {
        self.maybe_retest_first_link();
        let start_link = self.link;
        let mut last_error: Option<BoxError> = None;
        for offset in 0..self.chain.len() {
            let idx = (start_link + offset) % self.chain.len();
            let payload = serde_json::json!({
                "model": self.chain[idx],
                "messages": messages,
            });
            match self.transport.post(&self.endpoint, payload).await {
                Err(e) => {
                    last_error = Some(e);
                    let next = self.chain[(idx + 1) % self.chain.len()].clone();
                    self.switches.push(SwitchRecord {
                        from: self.chain[idx].clone(),
                        to: next,
                        reason: last_error
                            .as_ref()
                            .map(|e| e.to_string())
                            .unwrap_or_default(),
                    });
                    if idx == 0 {
                        // Arm the retest window even when a later link saves the turn.
                        self.link_failed_at = Some(Instant::now());
                    }
                    continue;
                }
                Ok(result) => {
                    let content = result
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_owned();
                    if idx != self.link {
                        self.link = idx;
                    }
                    if idx == 0 {
                        self.link_failed_at = None;
                    }
                    return Ok(ChatResult {
                        content,
                        model: self.chain[idx].clone(),
                        link: idx,
                    });
                }
            }
        }
        if self.link_failed_at.is_none() {
            self.link_failed_at = Some(Instant::now());
        }
        Err(last_error.expect("bug: chain exhausted without an error"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted stub transport: fails for the models in `fail_models`.
    struct StubTransport {
        fail_models: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl StubTransport {
        fn new(fail_models: &[&str]) -> Self {
            Self {
                fail_models: Mutex::new(fail_models.iter().map(|s| s.to_string()).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl GatewayTransport for StubTransport {
        async fn post(
            &self,
            _url: &str,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, BoxError> {
            let model = payload["model"].as_str().unwrap_or("").to_owned();
            self.calls
                .lock()
                .expect("stub calls lock poisoned")
                .push(model.clone());
            if self
                .fail_models
                .lock()
                .expect("stub fail-models lock poisoned")
                .contains(&model)
            {
                return Err("connection refused".into());
            }
            Ok(serde_json::json!({
                "choices": [{"message": {"content": format!("answer from {model}")}}]
            }))
        }
    }

    fn provider(fail: &[&str]) -> GatewayProvider {
        GatewayProvider::with_transport(
            GATEWAY_URL,
            MODELPOOL_CHAIN,
            Arc::new(StubTransport::new(fail)),
        )
    }

    #[tokio::test]
    async fn first_link_used_when_healthy() {
        let mut p = provider(&[]);
        let result = p
            .chat(&[crate::agent::ChatMessage {
                role: "user".to_owned(),
                content: "hi".to_owned(),
            }])
            .await
            .expect("first link must answer");
        assert_eq!(result.model, "freellmapi/auto");
        assert_eq!(result.link, 0);
        assert_eq!(result.content, "answer from freellmapi/auto");
        assert!(p.switches.is_empty());
    }

    #[tokio::test]
    async fn failover_to_next_link_and_switch_recorded() {
        let mut p = provider(&["freellmapi/auto"]);
        let result = p
            .chat(&[crate::agent::ChatMessage {
                role: "user".to_owned(),
                content: "hi".to_owned(),
            }])
            .await
            .expect("second link must answer");
        assert_eq!(result.model, "claude-sonnet-4-5");
        assert_eq!(result.link, 1);
        assert_eq!(p.switches.len(), 1);
        assert_eq!(p.switches[0].from, "freellmapi/auto");
        assert_eq!(p.switches[0].to, "claude-sonnet-4-5");
    }

    #[tokio::test]
    async fn chain_exhausted_raises_last_error() {
        let mut p = provider(MODELPOOL_CHAIN);
        let err = p
            .chat(&[crate::agent::ChatMessage {
                role: "user".to_owned(),
                content: "hi".to_owned(),
            }])
            .await
            .expect_err("exhausted chain must fail");
        assert!(err.to_string().contains("connection refused"));
        assert_eq!(p.switches.len(), MODELPOOL_CHAIN.len());
    }

    #[test]
    fn no_api_key_anywhere() {
        // The struct carries no key field, no key parameter, and no
        // environment read: the only key surface is this sentinel,
        // which always answers "None" (D9).
        assert_eq!(GatewayProvider::get_api_key("openai"), "None");
        assert_eq!(GatewayProvider::get_api_key("anthropic"), "None");
    }
}
