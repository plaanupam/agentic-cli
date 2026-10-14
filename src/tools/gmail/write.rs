use std::sync::Arc;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::client::{GmailClient, Outgoing};
use crate::policy::ToolSafetyTier;
use crate::tools::json_tool;

#[derive(Debug, Deserialize, JsonSchema)]
struct DraftArgs {
    to: String,
    subject: String,
    body: String,
    #[serde(default)]
    cc: String,
    #[serde(default)]
    bcc: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendArgs {
    to: String,
    subject: String,
    body: String,
    #[serde(default)]
    cc: String,
    #[serde(default)]
    bcc: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplyArgs {
    thread_id: String,
    to: String,
    subject: String,
    body: String,
    #[serde(default)]
    cc: String,
    #[serde(default)]
    in_reply_to: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ModifyArgs {
    message_id: String,
    #[serde(default)]
    add_labels: Vec<String>,
    #[serde(default)]
    remove_labels: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TrashArgs {
    message_id: String,
}

pub fn write_tools(client: Arc<GmailClient>) -> Vec<Arc<dyn crate::tools::Tool>> {
    vec![
        draft_tool(client.clone()),
        send_tool(client.clone()),
        reply_tool(client.clone()),
        modify_tool(client.clone()),
        trash_tool(client),
    ]
}

fn draft_tool(client: Arc<GmailClient>) -> Arc<dyn crate::tools::Tool> {
    bind::<DraftArgs>(
        client,
        "gmail_create_draft",
        "Create a Gmail draft. This does not send the message. Requires human approval.",
        |args| validate_compose(&args.to, &args.subject, &args.body),
        |client, args| {
            Box::pin(async move {
                client
                    .create_draft(&Outgoing {
                        to: args.to,
                        cc: args.cc,
                        bcc: args.bcc,
                        subject: args.subject,
                        body: args.body,
                        thread_id: None,
                        in_reply_to: None,
                    })
                    .await
            })
        },
    )
}

fn send_tool(client: Arc<GmailClient>) -> Arc<dyn crate::tools::Tool> {
    bind::<SendArgs>(
        client,
        "gmail_send_message",
        "Send an email. Requires human approval. Prefer gmail_create_draft unless the user explicitly asked to send.",
        |args| validate_compose(&args.to, &args.subject, &args.body),
        |client, args| {
            Box::pin(async move {
                client
                    .send(&Outgoing {
                        to: args.to,
                        cc: args.cc,
                        bcc: args.bcc,
                        subject: args.subject,
                        body: args.body,
                        thread_id: None,
                        in_reply_to: None,
                    })
                    .await
            })
        },
    )
}

fn reply_tool(client: Arc<GmailClient>) -> Arc<dyn crate::tools::Tool> {
    bind::<ReplyArgs>(
        client,
        "gmail_reply_thread",
        "Reply in an existing thread. Requires human approval. Pass the Gmail thread id and, when known, the Message-Id header as in_reply_to.",
        |args| {
            if args.thread_id.trim().is_empty() {
                return Err("thread_id is required".into());
            }
            validate_compose(&args.to, &args.subject, &args.body)
        },
        |client, args| {
            Box::pin(async move {
                client
                    .send(&Outgoing {
                        to: args.to,
                        cc: args.cc,
                        bcc: String::new(),
                        subject: args.subject,
                        body: args.body,
                        thread_id: Some(args.thread_id),
                        in_reply_to: if args.in_reply_to.trim().is_empty() {
                            None
                        } else {
                            Some(args.in_reply_to)
                        },
                    })
                    .await
            })
        },
    )
}

fn modify_tool(client: Arc<GmailClient>) -> Arc<dyn crate::tools::Tool> {
    bind::<ModifyArgs>(
        client,
        "gmail_modify_labels",
        "Add or remove label ids on a message. Requires human approval. Label ids come from gmail_list_labels, for example INBOX or UNREAD.",
        |args| {
            if args.message_id.trim().is_empty() {
                return Err("message_id is required".into());
            }
            if args.add_labels.is_empty() && args.remove_labels.is_empty() {
                return Err("add_labels or remove_labels must contain at least one label id".into());
            }
            Ok(())
        },
        |client, args| {
            Box::pin(async move {
                client
                    .modify_labels(&args.message_id, &args.add_labels, &args.remove_labels)
                    .await
            })
        },
    )
}

fn trash_tool(client: Arc<GmailClient>) -> Arc<dyn crate::tools::Tool> {
    bind::<TrashArgs>(
        client,
        "gmail_trash_message",
        "Move a message to trash. Requires human approval.",
        |args| {
            if args.message_id.trim().is_empty() {
                Err("message_id is required".into())
            } else {
                Ok(())
            }
        },
        |client, args| Box::pin(async move { client.trash(&args.message_id).await }),
    )
}

fn validate_compose(to: &str, subject: &str, body: &str) -> Result<(), String> {
    if !to.contains('@') {
        return Err("to must contain an email address".into());
    }
    if subject.trim().is_empty() {
        return Err("subject is required".into());
    }
    if body.trim().is_empty() {
        return Err("body is required".into());
    }
    Ok(())
}

fn bind<T>(
    client: Arc<GmailClient>,
    name: &'static str,
    description: &'static str,
    extra: impl Fn(&T) -> Result<(), String> + Send + Sync + 'static,
    run: impl Fn(Arc<GmailClient>, T) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync + 'static,
) -> Arc<dyn crate::tools::Tool>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static,
{
    let schema = schema_of::<T>();
    let extra = Arc::new(extra);
    let run = Arc::new(run);
    json_tool(
        name,
        description,
        ToolSafetyTier::HitlWrite,
        schema,
        Arc::new(move |args| {
            let parsed: T = serde_json::from_value(args.clone()).map_err(|err| err.to_string())?;
            extra(&parsed)
        }),
        Arc::new(move |value| {
            let client = client.clone();
            let run = Arc::clone(&run);
            Box::pin(async move {
                let args: T = serde_json::from_value(value)?;
                run(client, args).await
            })
        }),
    )
}

fn schema_of<T: schemars::JsonSchema>() -> Value {
    let settings = schemars::gen::SchemaSettings::draft07();
    let mut generator = schemars::gen::SchemaGenerator::new(settings);
    let schema = T::json_schema(&mut generator);
    let mut value = serde_json::to_value(&schema).unwrap_or_else(|_| serde_json::json!({"type": "object"}));
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    value
}
