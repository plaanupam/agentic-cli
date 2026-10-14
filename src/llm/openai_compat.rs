use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::LlmConfig;
use crate::llm::{ChatRequest, ChatResponse, LlmTool, ToolCall};

#[derive(Debug, Error)]
enum ProviderError {
    #[error("http {status}: {body}")]
    Status { status: u16, body: String },
    #[error("{0}")]
    Message(String),
}

pub struct OpenAiCompatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    api_style: String,
}

impl OpenAiCompatClient {
    pub fn new(config: &LlmConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.request_timeout_secs.max(30)))
            .user_agent("agentic-cli/0.1")
            .build()
            .context("building LLM HTTP client")?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            api_style: config.api_style.trim().to_ascii_lowercase(),
        })
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            builder
        } else {
            builder.bearer_auth(&self.api_key)
        }
    }

    async fn dispatch(
        &self,
        request: ChatRequest,
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<ChatResponse, ProviderError> {
        match self.api_style.as_str() {
            "ollama" => self.chat_ollama(request, deltas).await,
            "openai" => self.chat_openai(request, deltas).await,
            _ => match self.chat_openai(request.clone(), deltas.clone()).await {
                Err(ProviderError::Status { status: 404, .. }) => {
                    self.chat_ollama(request, deltas).await
                }
                other => other,
            },
        }
    }

    async fn chat_openai(
        &self,
        request: ChatRequest,
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<ChatResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = json!({
            "model": request.model,
            "messages": messages_openai(&request.messages),
            "tools": tools_openai(&request.tools),
            "tool_choice": if request.tools.is_empty() { Value::Null } else { json!("auto") },
            "stream": true,
        });
        let body = strip_nulls(body);
        let response = self
            .authed(self.http.post(url).json(&body))
            .send()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        self.read_openai_stream(response, &deltas, &request).await
    }

    async fn chat_ollama(
        &self,
        request: ChatRequest,
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<ChatResponse, ProviderError> {
        let url = ollama_url(&self.base_url, "/api/chat");
        let body = json!({
            "model": request.model,
            "messages": messages_ollama(&request.messages),
            "tools": tools_openai(&request.tools),
            "stream": true,
        });
        let response = self
            .authed(self.http.post(url).json(&body))
            .send()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status {
                status,
                body: clip(&text, 800),
            });
        }
        let mut state = StreamState::default();
        let mut buf = String::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| ProviderError::Message(err.to_string()))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            consume_ndjson(&mut buf, &mut state, &deltas, true);
        }
        if !buf.trim().is_empty() {
            buf.push('\n');
            consume_ndjson(&mut buf, &mut state, &deltas, true);
        }
        Ok(state.finish(&request))
    }

    async fn read_openai_stream(
        &self,
        response: reqwest::Response,
        deltas: &mpsc::UnboundedSender<String>,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status {
                status,
                body: clip(&text, 800),
            });
        }
        let mut state = StreamState::default();
        let mut buf = String::new();
        let mut mode = StreamMode::Unknown;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| ProviderError::Message(err.to_string()))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if matches!(mode, StreamMode::Unknown) && buf.contains("data:") {
                mode = StreamMode::Sse;
            }
            if matches!(mode, StreamMode::Sse) {
                consume_sse(&mut buf, &mut state, deltas);
            }
        }
        if matches!(mode, StreamMode::Unknown) {
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                let value: Value = serde_json::from_str(trimmed)
                    .map_err(|err| ProviderError::Message(format!("invalid JSON response: {err}")))?;
                apply_json(&value, &mut state, deltas);
            }
        } else if !buf.trim().is_empty() {
            consume_sse(&mut buf, &mut state, deltas);
        }
        Ok(state.finish(request))
    }

    async fn list_inner(&self) -> Result<Vec<String>, ProviderError> {
        match self.api_style.as_str() {
            "ollama" => self.list_ollama().await,
            "openai" => self.list_openai().await,
            _ => match self.list_openai().await {
                Err(ProviderError::Status { status: 404, .. }) => self.list_ollama().await,
                other => other,
            },
        }
    }

    async fn list_openai(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = self
            .authed(self.http.get(url))
            .send()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Status {
                status: status.as_u16(),
                body: clip(&text, 800),
            });
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        let mut names = Vec::new();
        if let Some(data) = value.get("data").and_then(|v| v.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    names.push(id.to_string());
                }
            }
        }
        Ok(names)
    }

    async fn list_ollama(&self) -> Result<Vec<String>, ProviderError> {
        let url = ollama_url(&self.base_url, "/api/tags");
        let response = self
            .authed(self.http.get(url))
            .send()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Status {
                status: status.as_u16(),
                body: clip(&text, 800),
            });
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| ProviderError::Message(err.to_string()))?;
        let mut names = Vec::new();
        if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
            for item in models {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

#[async_trait::async_trait]
impl crate::llm::LlmProvider for OpenAiCompatClient {
    async fn chat(
        &self,
        request: ChatRequest,
        deltas: mpsc::UnboundedSender<String>,
    ) -> anyhow::Result<ChatResponse> {
        self.dispatch(request, deltas)
            .await
            .map_err(|err| annotate(anyhow::Error::from(err)))
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        self.list_inner()
            .await
            .map_err(|err| annotate(anyhow::Error::from(err)))
    }
}

#[derive(Default)]
struct AccCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamState {
    content: String,
    calls: Vec<AccCall>,
    prompt_tokens: u32,
    completion_tokens: u32,
}

impl StreamState {
    fn finish(self, request: &ChatRequest) -> ChatResponse {
        let mut prompt_tokens = self.prompt_tokens;
        let mut completion_tokens = self.completion_tokens;
        if prompt_tokens == 0 {
            let chars: usize = request.messages.iter().map(|m| m.content.len()).sum();
            prompt_tokens = (chars / 4) as u32;
        }
        if completion_tokens == 0 {
            completion_tokens = (self.content.len() / 4) as u32;
        }
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}_{}", &Uuid::new_v4().to_string()[..8])
                } else {
                    call.id
                },
                name: call.name,
                arguments: if call.arguments.is_empty() {
                    "{}".into()
                } else {
                    call.arguments
                },
            })
            .collect();
        ChatResponse {
            content: self.content,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        }
    }
}

enum StreamMode {
    Unknown,
    Sse,
}

fn consume_sse(buf: &mut String, state: &mut StreamState, deltas: &mpsc::UnboundedSender<String>) {
    while let Some(idx) = buf.find('\n') {
        let mut line: String = buf.drain(..=idx).collect();
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            apply_json(&value, state, deltas);
        }
    }
}

fn consume_ndjson(
    buf: &mut String,
    state: &mut StreamState,
    deltas: &mpsc::UnboundedSender<String>,
    ollama: bool,
) {
    while let Some(idx) = buf.find('\n') {
        let mut line: String = buf.drain(..=idx).collect();
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if ollama {
                apply_ollama(&value, state, deltas);
            } else {
                apply_json(&value, state, deltas);
            }
        }
    }
}

fn apply_ollama(value: &Value, state: &mut StreamState, deltas: &mpsc::UnboundedSender<String>) {
    if let Some(count) = value.get("prompt_eval_count").and_then(|v| v.as_u64()) {
        state.prompt_tokens = count as u32;
    }
    if let Some(count) = value.get("eval_count").and_then(|v| v.as_u64()) {
        state.completion_tokens = count as u32;
    }
    if let Some(message) = value.get("message") {
        apply_delta(message, state, deltas);
    }
}

fn apply_json(value: &Value, state: &mut StreamState, deltas: &mpsc::UnboundedSender<String>) {
    if let Some(usage) = value.get("usage") {
        if let Some(count) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
            state.prompt_tokens = count as u32;
        }
        if let Some(count) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
            state.completion_tokens = count as u32;
        }
    }
    let Some(choice) = value.get("choices").and_then(|v| v.get(0)) else {
        return;
    };
    if let Some(delta) = choice.get("delta") {
        apply_delta(delta, state, deltas);
    }
    if let Some(message) = choice.get("message") {
        apply_delta(message, state, deltas);
    }
}

fn apply_delta(delta: &Value, state: &mut StreamState, deltas: &mpsc::UnboundedSender<String>) {
    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            state.content.push_str(text);
            let _ = deltas.send(text.to_string());
        }
    }
    for key in ["reasoning", "reasoning_content"] {
        if let Some(text) = delta.get(key).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                let _ = deltas.send(text.to_string());
            }
        }
    }
    if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for (fallback, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or(fallback as u64) as usize;
            if state.calls.len() <= index {
                state.calls.resize_with(index + 1, AccCall::default);
            }
            let slot = &mut state.calls[index];
            if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    slot.id = id.to_string();
                }
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                    merge_name(&mut slot.name, name);
                }
                if let Some(arguments) = function.get("arguments") {
                    slot.arguments.push_str(&arg_fragment(arguments));
                }
            }
        }
    }
}

fn merge_name(slot: &mut String, incoming: &str) {
    if incoming.is_empty() {
        return;
    }
    if slot.is_empty() || slot == incoming || incoming.starts_with(slot.as_str()) {
        *slot = incoming.to_string();
    } else if !slot.ends_with(incoming) {
        slot.push_str(incoming);
    }
}

pub(crate) fn arg_fragment(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn messages_openai(messages: &[crate::llm::ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut value = json!({
                "role": message.role,
                "content": message.content,
            });
            if let Some(id) = &message.tool_call_id {
                value["tool_call_id"] = json!(id);
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = json!(message
                    .tool_calls
                    .iter()
                    .map(|call| json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments,
                        }
                    }))
                    .collect::<Vec<_>>());
            }
            value
        })
        .collect()
}

fn messages_ollama(messages: &[crate::llm::ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut value = json!({
                "role": message.role,
                "content": message.content,
            });
            if let Some(name) = &message.name {
                value["name"] = json!(name);
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = json!(message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        let arguments = serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!(call.arguments));
                        json!({
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": arguments,
                            }
                        })
                    })
                    .collect::<Vec<_>>());
            }
            value
        })
        .collect()
}

fn tools_openai(tools: &[LlmTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }
            })
        })
        .collect()
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                if !child.is_null() {
                    out.insert(key, strip_nulls(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

fn ollama_url(base: &str, path: &str) -> String {
    let origin = base.strip_suffix("/v1").unwrap_or(base);
    format!("{}{path}", origin.trim_end_matches('/'))
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

fn annotate(err: anyhow::Error) -> anyhow::Error {
    let text = err.to_string();
    if text.contains("Connection refused")
        || text.contains("error trying to connect")
        || text.contains("timed out")
    {
        anyhow::anyhow!(
            "{text}\nHint: start a local model server, for example `ollama serve`, and check llm.base_url."
        )
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_fragments_concatenate() {
        let mut state = StreamState::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_delta(
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "gmail_search", "arguments": "{\"query\":"}
                }]
            }),
            &mut state,
            &tx,
        );
        apply_delta(
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\"is:unread\"}"}
                }]
            }),
            &mut state,
            &tx,
        );
        assert_eq!(state.calls[0].name, "gmail_search");
        assert_eq!(state.calls[0].arguments, "{\"query\":\"is:unread\"}");
        let parsed: Value = serde_json::from_str(&state.calls[0].arguments).unwrap();
        assert_eq!(parsed["query"], "is:unread");
    }

    #[test]
    fn object_arguments_are_stringified() {
        assert_eq!(
            arg_fragment(&json!({"query": "is:unread"})),
            "{\"query\":\"is:unread\"}"
        );
    }
}
