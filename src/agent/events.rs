use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::policy::ApprovalDecision;
use crate::util::truncate_chars;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Thinking,
    CallingTool { name: String },
    WaitingForHuman,
    Completed,
    Failed,
}

impl AgentStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "idle".into(),
            Self::Thinking => "thinking".into(),
            Self::CallingTool { name } => format!("tool {name}"),
            Self::WaitingForHuman => "waiting for approval".into(),
            Self::Completed => "done".into(),
            Self::Failed => "failed".into(),
        }
    }
}

#[derive(Clone)]
pub struct ApprovalTicket {
    pub request_id: String,
    pub agent_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub preview: String,
    responder: Arc<Mutex<Option<oneshot::Sender<ApprovalDecision>>>>,
}

impl ApprovalTicket {
    pub fn new(
        agent_id: impl Into<String>,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        preview: impl Into<String>,
    ) -> (Self, oneshot::Receiver<ApprovalDecision>) {
        let (tx, rx) = oneshot::channel();
        let ticket = Self {
            request_id: Uuid::new_v4().to_string(),
            agent_id: agent_id.into(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            preview: preview.into(),
            responder: Arc::new(Mutex::new(Some(tx))),
        };
        (ticket, rx)
    }

    pub fn respond(self, decision: ApprovalDecision) -> Result<(), ApprovalDecision> {
        let mut guard = match self.responder.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.take() {
            Some(tx) => tx.send(decision),
            None => Err(decision),
        }
    }
}

#[derive(Clone)]
pub enum AgentEvent {
    Spawned {
        agent_id: String,
        parent_id: Option<String>,
        name: String,
        role: String,
    },
    StatusChanged {
        agent_id: String,
        status: AgentStatus,
    },
    ThinkingDelta {
        agent_id: String,
        delta: String,
    },
    Thought {
        agent_id: String,
        text: String,
    },
    ToolCallRequested {
        agent_id: String,
        call_id: String,
        tool_name: String,
        arguments: String,
    },
    ApprovalRequired(ApprovalTicket),
    ToolCallCompleted {
        agent_id: String,
        call_id: String,
        tool_name: String,
        result_preview: String,
        latency_ms: u64,
    },
    Finished {
        agent_id: String,
        summary: String,
    },
    Failed {
        agent_id: String,
        error: String,
    },
    TokenUsage {
        agent_id: String,
        prompt_tokens: u32,
        completion_tokens: u32,
    },
}

impl AgentEvent {
    pub fn agent_id(&self) -> &str {
        match self {
            Self::Spawned { agent_id, .. }
            | Self::StatusChanged { agent_id, .. }
            | Self::ThinkingDelta { agent_id, .. }
            | Self::Thought { agent_id, .. }
            | Self::ToolCallRequested { agent_id, .. }
            | Self::ToolCallCompleted { agent_id, .. }
            | Self::Finished { agent_id, .. }
            | Self::Failed { agent_id, .. }
            | Self::TokenUsage { agent_id, .. } => agent_id,
            Self::ApprovalRequired(ticket) => &ticket.agent_id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Spawned { .. } => "spawned",
            Self::StatusChanged { .. } => "status",
            Self::ThinkingDelta { .. } => "thinking",
            Self::Thought { .. } => "thought",
            Self::ToolCallRequested { .. } => "tool_requested",
            Self::ApprovalRequired(_) => "approval_required",
            Self::ToolCallCompleted { .. } => "tool_completed",
            Self::Finished { .. } => "finished",
            Self::Failed { .. } => "failed",
            Self::TokenUsage { .. } => "tokens",
        }
    }

    pub fn persist(&self) -> bool {
        !matches!(self, Self::ThinkingDelta { .. } | Self::StatusChanged { .. })
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Spawned {
                agent_id,
                parent_id,
                name,
                role,
            } => json!({
                "summary": format!("{role} {name} started"),
                "agent_id": agent_id,
                "parent_id": parent_id,
                "name": name,
                "role": role,
            }),
            Self::StatusChanged { agent_id, status } => json!({
                "summary": status.label(),
                "agent_id": agent_id,
            }),
            Self::ThinkingDelta { agent_id, delta } => json!({
                "summary": truncate_chars(delta, 120),
                "agent_id": agent_id,
            }),
            Self::Thought { agent_id, text } => json!({
                "summary": truncate_chars(text, 240),
                "agent_id": agent_id,
                "text": text,
            }),
            Self::ToolCallRequested {
                agent_id,
                call_id,
                tool_name,
                arguments,
            } => json!({
                "summary": format!("{tool_name} {arguments}"),
                "agent_id": agent_id,
                "call_id": call_id,
                "tool_name": tool_name,
                "arguments": arguments,
            }),
            Self::ApprovalRequired(ticket) => json!({
                "summary": format!("approval required for {}", ticket.tool_name),
                "agent_id": ticket.agent_id,
                "request_id": ticket.request_id,
                "call_id": ticket.call_id,
                "tool_name": ticket.tool_name,
                "arguments": ticket.arguments,
                "preview": ticket.preview,
            }),
            Self::ToolCallCompleted {
                agent_id,
                call_id,
                tool_name,
                result_preview,
                latency_ms,
            } => json!({
                "summary": format!("{tool_name} finished in {latency_ms}ms"),
                "agent_id": agent_id,
                "call_id": call_id,
                "tool_name": tool_name,
                "result_preview": result_preview,
                "latency_ms": latency_ms,
            }),
            Self::Finished { agent_id, summary } => json!({
                "summary": summary,
                "agent_id": agent_id,
            }),
            Self::Failed { agent_id, error } => json!({
                "summary": error,
                "agent_id": agent_id,
            }),
            Self::TokenUsage {
                agent_id,
                prompt_tokens,
                completion_tokens,
            } => json!({
                "summary": format!("prompt={prompt_tokens} completion={completion_tokens}"),
                "agent_id": agent_id,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            }),
        }
    }
}
