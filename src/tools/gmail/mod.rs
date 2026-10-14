mod auth;
mod client;
mod read;
mod write;

use std::sync::Arc;

pub use auth::{login, logout, status};
pub use client::GmailClient;

use crate::tools::ToolRegistry;

pub fn all_tools(client: Arc<GmailClient>) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for tool in read::read_tools(client.clone()) {
        registry.push(tool);
    }
    for tool in write::write_tools(client) {
        registry.push(tool);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ToolSafetyTier;
    use serde_json::json;

    #[test]
    fn read_tools_are_autonomous_and_writes_are_gated() {
        let tools = all_tools(Arc::new(GmailClient::disconnected()));
        for name in [
            "gmail_search",
            "gmail_get_thread",
            "gmail_get_message",
            "gmail_list_labels",
        ] {
            let tool = tools.get(name).expect(name);
            assert_eq!(tool.safety_tier(), ToolSafetyTier::AutonomousRead);
        }
        for name in [
            "gmail_create_draft",
            "gmail_send_message",
            "gmail_reply_thread",
            "gmail_modify_labels",
            "gmail_trash_message",
        ] {
            let tool = tools.get(name).expect(name);
            assert_eq!(tool.safety_tier(), ToolSafetyTier::HitlWrite);
        }
        let search = tools.get("gmail_search").unwrap();
        let schema = search.parameters_schema();
        assert!(schema["properties"]["query"].is_object() || schema.get("properties").is_some());
        let send = tools.get("gmail_send_message").unwrap();
        assert!(send.validate(&json!({"subject": "Hi", "body": "x"})).is_err());
        send.validate(&json!({"to": "a@b.com", "subject": "Hi", "body": "Hello"}))
            .unwrap();
    }
}
