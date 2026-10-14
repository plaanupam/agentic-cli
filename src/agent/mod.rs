use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::events::{AgentEvent, AgentStatus, ApprovalTicket};
use crate::agent::supervisor::SpawnRequest;
use crate::config::Config;
use crate::llm::{ChatMessage, ChatRequest, LlmProvider, LlmTool, ToolCall};
use crate::policy::{ApprovalDecision, ToolSafetyTier};
use crate::storage::{Store, idempotency_key};
use crate::tools::{ToolRegistry, preview_tool_call};
use crate::util::truncate_chars;

pub mod events;
pub mod supervisor;

pub type SpawnFactory =
    Arc<dyn Fn(SpawnRequest) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;

#[derive(Clone)]
pub struct RuntimeDeps {
    pub config: Config,
    pub llm: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub events: broadcast::Sender<AgentEvent>,
    pub approvals: mpsc::UnboundedSender<ApprovalTicket>,
    pub store: Arc<Store>,
    pub session_id: String,
    pub cancel: CancellationToken,
}

pub struct AgentRuntime {
    id: String,
    name: String,
    role: String,
    parent_id: Option<String>,
    model: String,
    max_steps: usize,
    max_tool_retries: usize,
    max_chars: usize,
    messages: Vec<ChatMessage>,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    events: broadcast::Sender<AgentEvent>,
    approvals: mpsc::UnboundedSender<ApprovalTicket>,
    store: Arc<Store>,
    session_id: String,
    cancel: CancellationToken,
    spawn: Option<SpawnFactory>,
    repairs: usize,
    recent: Vec<String>,
    spawns: usize,
}

struct CallOutcome {
    content: String,
    stop: bool,
}

impl CallOutcome {
    fn cont(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            stop: false,
        }
    }

    fn halt(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            stop: true,
        }
    }
}

impl AgentRuntime {
    pub fn supervisor(deps: RuntimeDeps) -> Self {
        let spawn = Some(supervisor::spawn_factory(deps.clone()));
        let mut runtime = Self::build(deps, "supervisor".into(), "supervisor", None, true);
        runtime.spawn = spawn;
        runtime
    }

    pub fn worker(deps: RuntimeDeps, parent_id: String, name: String) -> Self {
        let short = &Uuid::new_v4().to_string()[..4];
        let name = if name.trim().is_empty() {
            format!("worker-{short}")
        } else {
            format!("{name}-{short}")
        };
        Self::build(deps, name, "worker", Some(parent_id), false)
    }

    fn build(
        deps: RuntimeDeps,
        name: String,
        role: &str,
        parent_id: Option<String>,
        supervisor_agent: bool,
    ) -> Self {
        let prompt = if supervisor_agent {
            supervisor::SUPERVISOR_PROMPT
        } else {
            supervisor::WORKER_PROMPT
        };
        let model = if supervisor_agent {
            deps.config.llm.orchestrator_model.clone()
        } else {
            deps.config.llm.worker_model.clone()
        };
        let max_steps = if supervisor_agent {
            deps.config.agent.max_supervisor_steps
        } else {
            deps.config.agent.max_worker_steps
        };
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            role: role.into(),
            parent_id,
            model,
            max_steps: max_steps.max(1),
            max_tool_retries: deps.config.agent.max_tool_retries,
            max_chars: deps.config.llm.max_context_tokens.saturating_mul(4).max(4000),
            messages: vec![ChatMessage::system(prompt)],
            llm: deps.llm,
            tools: deps.tools,
            events: deps.events,
            approvals: deps.approvals,
            store: deps.store,
            session_id: deps.session_id,
            cancel: deps.cancel,
            spawn: None,
            repairs: 0,
            recent: Vec::new(),
            spawns: 0,
        }
    }

    pub async fn run(mut self, goal: String) -> anyhow::Result<String> {
        self.emit(AgentEvent::Spawned {
            agent_id: self.id.clone(),
            parent_id: self.parent_id.clone(),
            name: self.name.clone(),
            role: self.role.clone(),
        });
        self.messages.push(ChatMessage::user(goal));
        let mut steps = 0;
        loop {
            if self.cancel.is_cancelled() {
                let summary = "cancelled".to_string();
                self.finish(&summary);
                return Ok(summary);
            }
            if steps >= self.max_steps {
                let summary = format!("stopped after {} steps", self.max_steps);
                self.finish(&summary);
                return Ok(summary);
            }
            steps += 1;
            self.set_status(AgentStatus::Thinking);
            let response = match self.complete().await {
                Ok(response) => response,
                Err(err) => {
                    self.fail(&err.to_string());
                    return Err(err);
                }
            };
            if response.tool_calls.is_empty() {
                let text = if response.content.trim().is_empty() {
                    "(no response)".to_string()
                } else {
                    response.content.clone()
                };
                self.messages
                    .push(ChatMessage::assistant(text.clone(), Vec::new()));
                self.finish(&text);
                return Ok(text);
            }
            self.messages.push(ChatMessage::assistant(
                response.content.clone(),
                response.tool_calls.clone(),
            ));
            for call in response.tool_calls {
                let outcome = self.handle_call(call.clone()).await;
                self.push_tool_result(&call, outcome.content.clone());
                if outcome.stop {
                    self.finish(&outcome.content);
                    return Ok(outcome.content);
                }
            }
        }
    }

    async fn complete(&mut self) -> anyhow::Result<crate::llm::ChatResponse> {
        self.compact();
        let mut tools: Vec<LlmTool> = self
            .tools
            .iter()
            .map(|tool| LlmTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            })
            .collect();
        if self.spawn.is_some() {
            tools.push(supervisor::spawn_tool());
        }
        let request = ChatRequest {
            model: self.model.clone(),
            messages: self.messages.clone(),
            tools,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let llm = Arc::clone(&self.llm);
        let agent_id = self.id.clone();
        let events = self.events.clone();
        let handle = tokio::spawn(async move { llm.chat(request, tx).await });
        while let Some(delta) = rx.recv().await {
            let _ = events.send(AgentEvent::ThinkingDelta {
                agent_id: agent_id.clone(),
                delta,
            });
        }
        let response = handle
            .await
            .map_err(|err| anyhow::anyhow!("llm task failed: {err}"))??;
        let _ = self.events.send(AgentEvent::TokenUsage {
            agent_id: self.id.clone(),
            prompt_tokens: response.prompt_tokens,
            completion_tokens: response.completion_tokens,
        });
        if !response.content.is_empty() {
            let _ = self.events.send(AgentEvent::Thought {
                agent_id: self.id.clone(),
                text: response.content.clone(),
            });
        }
        Ok(response)
    }

    async fn handle_call(&mut self, call: ToolCall) -> CallOutcome {
        let signature = format!("{}:{}", call.name, call.arguments);
        self.emit(AgentEvent::ToolCallRequested {
            agent_id: self.id.clone(),
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: truncate_chars(&call.arguments, 2000),
        });
        if self.note_repeat(&signature) {
            let text = format!("stopped: repeated tool call {}", call.name);
            self.emit_completed(&call.id, &call.name, &text, 0);
            return CallOutcome::halt(text);
        }
        if call.name == "spawn_subagent" {
            return self.handle_spawn(call).await;
        }
        let Some(tool) = self.tools.get(&call.name) else {
            let text = format!("Unknown tool `{}`.", call.name);
            self.emit_completed(&call.id, &call.name, &text, 0);
            return CallOutcome::cont(text);
        };
        let args = match serde_json::from_str::<Value>(&call.arguments) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return self.invalid_args(&call, tool.as_ref(), "arguments must be a JSON object");
            }
            Err(err) => return self.invalid_args(&call, tool.as_ref(), &err.to_string()),
        };
        if let Err(err) = tool.validate(&args) {
            return self.invalid_args(&call, tool.as_ref(), &err);
        }
        self.repairs = 0;
        let mut args = args;
        if tool.safety_tier() == ToolSafetyTier::HitlWrite {
            let key = idempotency_key(&self.session_id, tool.name(), &args);
            if let Some(cached) = self.cached(&key) {
                let text = format!("[idempotent replay] {cached}");
                self.emit_completed(&call.id, tool.name(), &text, 0);
                return CallOutcome::cont(text);
            }
            match self.request_approval(&call.id, tool.name(), &args).await {
                ApprovalDecision::Approve => {}
                ApprovalDecision::ApproveWithModifiedArgs(edited) => {
                    if let Err(err) = tool.validate(&edited) {
                        let text = format!("Edited arguments were rejected: {err}");
                        self.emit_completed(&call.id, tool.name(), &text, 0);
                        return CallOutcome::cont(text);
                    }
                    args = edited;
                }
                ApprovalDecision::Reject { reason } => {
                    let text = format!("Human rejected this write: {reason}");
                    self.emit_completed(&call.id, tool.name(), &text, 0);
                    return CallOutcome::cont(text);
                }
            }
            let key = idempotency_key(&self.session_id, tool.name(), &args);
            if let Some(cached) = self.cached(&key) {
                let text = format!("[idempotent replay] {cached}");
                self.emit_completed(&call.id, tool.name(), &text, 0);
                return CallOutcome::cont(text);
            }
            self.set_status(AgentStatus::CallingTool {
                name: tool.name().to_string(),
            });
            let started = std::time::Instant::now();
            let executed = tool.execute(args.clone()).await;
            let latency = started.elapsed().as_millis() as u64;
            match executed {
                Ok(text) => {
                    if let Err(err) = self.store.put_idempotent(&key, tool.name(), &text) {
                        tracing::warn!(%err, "failed to record idempotency key");
                    }
                    self.emit_completed(&call.id, tool.name(), &text, latency);
                    CallOutcome::cont(text)
                }
                Err(err) => {
                    let text = format!("Tool {} failed: {err}", tool.name());
                    self.emit_completed(&call.id, tool.name(), &text, latency);
                    CallOutcome::cont(text)
                }
            }
        } else {
            self.set_status(AgentStatus::CallingTool {
                name: tool.name().to_string(),
            });
            let started = std::time::Instant::now();
            let executed = tool.execute(args).await;
            let latency = started.elapsed().as_millis() as u64;
            match executed {
                Ok(text) => {
                    self.emit_completed(&call.id, tool.name(), &text, latency);
                    CallOutcome::cont(text)
                }
                Err(err) => {
                    let text = format!("Tool {} failed: {err}", tool.name());
                    self.emit_completed(&call.id, tool.name(), &text, latency);
                    CallOutcome::cont(text)
                }
            }
        }
    }

    async fn handle_spawn(&mut self, call: ToolCall) -> CallOutcome {
        let Some(factory) = self.spawn.clone() else {
            let text = "spawn_subagent is only available to the supervisor.".to_string();
            self.emit_completed(&call.id, &call.name, &text, 0);
            return CallOutcome::cont(text);
        };
        #[derive(Deserialize)]
        struct SpawnArgs {
            task: String,
            #[serde(default)]
            name: String,
        }
        let parsed = match serde_json::from_str::<SpawnArgs>(&call.arguments) {
            Ok(value) => value,
            Err(err) => {
                return self.invalid_spawn(&call, &err.to_string());
            }
        };
        if parsed.task.trim().is_empty() {
            let text = "spawn_subagent requires a non-empty task.".to_string();
            self.emit_completed(&call.id, &call.name, &text, 0);
            return CallOutcome::cont(text);
        }
        self.spawns += 1;
        if self.spawns > 4 {
            let text = "spawn limit reached (4). Continue with the information already gathered.".to_string();
            self.emit_completed(&call.id, &call.name, &text, 0);
            return CallOutcome::cont(text);
        }
        self.set_status(AgentStatus::CallingTool {
            name: "spawn_subagent".into(),
        });
        let started = std::time::Instant::now();
        let result = factory(SpawnRequest {
            parent_id: self.id.clone(),
            name: parsed.name,
            task: parsed.task,
        })
        .await;
        let latency = started.elapsed().as_millis() as u64;
        match result {
            Ok(summary) => {
                let summary = truncate_chars(&summary, 6000);
                self.emit_completed(&call.id, "spawn_subagent", &summary, latency);
                CallOutcome::cont(summary)
            }
            Err(err) => {
                let text = format!("spawn_subagent failed: {err}");
                self.emit_completed(&call.id, "spawn_subagent", &text, latency);
                CallOutcome::cont(text)
            }
        }
    }

    fn invalid_args(&mut self, call: &ToolCall, tool: &dyn crate::tools::Tool, err: &str) -> CallOutcome {
        self.repairs += 1;
        let text = format!(
            "Invalid arguments for {}: {err}\nReceived: {}\nSchema: {}\nCall the tool again with JSON matching the schema.",
            tool.name(),
            truncate_chars(&call.arguments, 500),
            tool.parameters_schema()
        );
        self.emit_completed(&call.id, tool.name(), &text, 0);
        if self.repairs > self.max_tool_retries {
            CallOutcome::halt(format!(
                "{text}\nStopped after {} invalid tool calls.",
                self.max_tool_retries
            ))
        } else {
            CallOutcome::cont(text)
        }
    }

    fn invalid_spawn(&mut self, call: &ToolCall, err: &str) -> CallOutcome {
        self.repairs += 1;
        let text = format!(
            "Invalid arguments for spawn_subagent: {err}\nSchema: {{\"type\":\"object\",\"required\":[\"task\"],\"properties\":{{\"task\":{{\"type\":\"string\"}},\"name\":{{\"type\":\"string\"}}}}}}"
        );
        self.emit_completed(&call.id, &call.name, &text, 0);
        if self.repairs > self.max_tool_retries {
            CallOutcome::halt(text)
        } else {
            CallOutcome::cont(text)
        }
    }

    async fn request_approval(&self, call_id: &str, tool_name: &str, args: &Value) -> ApprovalDecision {
        self.set_status(AgentStatus::WaitingForHuman);
        let preview = preview_tool_call(tool_name, args);
        let (ticket, rx) = ApprovalTicket::new(
            self.id.clone(),
            call_id,
            tool_name,
            args.clone(),
            preview,
        );
        if self.approvals.send(ticket.clone()).is_err() {
            let _ = ticket.respond(ApprovalDecision::Reject {
                reason: "approval listener is not running".into(),
            });
            return ApprovalDecision::Reject {
                reason: "approval listener is not running".into(),
            };
        }
        let _ = self
            .events
            .send(AgentEvent::ApprovalRequired(ticket));
        match rx.await {
            Ok(decision) => decision,
            Err(_) => ApprovalDecision::Reject {
                reason: "approval was cancelled".into(),
            },
        }
    }

    fn cached(&self, key: &str) -> Option<String> {
        match self.store.get_idempotent(key) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(%err, "idempotency lookup failed");
                None
            }
        }
    }

    fn note_repeat(&mut self, signature: &str) -> bool {
        self.recent.push(signature.to_string());
        if self.recent.len() > 8 {
            self.recent.remove(0);
        }
        self.recent.iter().filter(|item| item == &signature).count() >= 3
    }

    fn push_tool_result(&mut self, call: &ToolCall, content: String) {
        self.messages.push(ChatMessage::tool_result(
            call.id.clone(),
            call.name.clone(),
            truncate_chars(&content, 12_000),
        ));
    }

    fn compact(&mut self) {
        while message_size(&self.messages) > self.max_chars && self.messages.len() > 4 {
            if self
                .messages
                .get(1)
                .map(|message| !message.tool_calls.is_empty())
                .unwrap_or(false)
            {
                self.messages.remove(1);
                while self
                    .messages
                    .get(1)
                    .map(|message| message.role == "tool")
                    .unwrap_or(false)
                {
                    self.messages.remove(1);
                }
            } else {
                self.messages.remove(1);
            }
        }
    }

    fn emit_completed(&self, call_id: &str, tool_name: &str, result: &str, latency_ms: u64) {
        self.emit(AgentEvent::ToolCallCompleted {
            agent_id: self.id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            result_preview: truncate_chars(result, 500),
            latency_ms,
        });
    }

    fn set_status(&self, status: AgentStatus) {
        self.emit(AgentEvent::StatusChanged {
            agent_id: self.id.clone(),
            status,
        });
    }

    fn finish(&self, summary: &str) {
        self.set_status(AgentStatus::Completed);
        self.emit(AgentEvent::Finished {
            agent_id: self.id.clone(),
            summary: summary.to_string(),
        });
    }

    fn fail(&self, error: &str) {
        self.set_status(AgentStatus::Failed);
        self.emit(AgentEvent::Failed {
            agent_id: self.id.clone(),
            error: error.to_string(),
        });
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}

fn message_size(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len() + call.name.len())
                    .sum::<usize>()
        })
        .sum()
}

pub fn spawn_persist(
    store: Arc<Store>,
    session_id: String,
    mut rx: broadcast::Receiver<AgentEvent>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event.persist() {
                        continue;
                    }
                    let payload = event.to_json().to_string();
                    if let Err(err) =
                        store.append_event(&session_id, event.agent_id(), event.kind(), &payload)
                    {
                        tracing::warn!(%err, "failed to persist event");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "event log lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
