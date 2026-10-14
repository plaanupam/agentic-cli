use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::policy::ToolSafetyTier;
use crate::util::truncate_chars;

#[macro_export]
macro_rules! tool_schema {
    ($ty:ty) => {{
        let root = schemars::schema_for!($ty);
        let mut value = serde_json::to_value(&root).expect("tool schema serializes");
        if let Some(obj) = value.as_object_mut() {
            obj.remove("$schema");
            obj.remove("title");
        }
        value
    }};
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn safety_tier(&self) -> ToolSafetyTier;
    fn parameters_schema(&self) -> Value;
    fn validate(&self, args: &Value) -> Result<(), String>;
    async fn execute(&self, args: Value) -> anyhow::Result<String>;
}

pub struct JsonTool {
    name: &'static str,
    description: &'static str,
    tier: ToolSafetyTier,
    schema: Value,
    validate_fn: Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>,
    run: Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>,
}

pub fn json_tool(
    name: &'static str,
    description: &'static str,
    tier: ToolSafetyTier,
    schema: Value,
    validate_fn: Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>,
    run: Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>,
) -> Arc<dyn Tool> {
    Arc::new(JsonTool {
        name,
        description,
        tier,
        schema,
        validate_fn,
        run,
    })
}

pub fn check<T: DeserializeOwned>(args: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(args.clone())
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[async_trait::async_trait]
impl Tool for JsonTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn safety_tier(&self) -> ToolSafetyTier {
        self.tier
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn validate(&self, args: &Value) -> Result<(), String> {
        (self.validate_fn)(args)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<String> {
        (self.run)(args).await
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl ToolRegistry {
    pub fn push(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.order.push(name.clone());
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Tool>> + '_ {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name).cloned())
    }
}

pub fn preview_tool_call(name: &str, args: &Value) -> String {
    let preview = match name {
        "gmail_send_message" | "gmail_create_draft" | "gmail_reply_thread" => format!(
            "To: {}\nCc: {}\nBcc: {}\nSubject: {}\nThread: {}\nIn-Reply-To: {}\n\n{}",
            header_field(args, "to"),
            header_field(args, "cc"),
            header_field(args, "bcc"),
            header_field(args, "subject"),
            header_field(args, "thread_id"),
            header_field(args, "in_reply_to"),
            text_field(args, "body"),
        ),
        _ => serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string()),
    };
    truncate_chars(&preview, 4000)
}

fn text_field(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string()
}

fn header_field(args: &Value, key: &str) -> String {
    sanitize_header(&text_field(args, key))
}

pub fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

pub mod gmail;
