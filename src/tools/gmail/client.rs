use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use reqwest::Method;
use serde_json::{Value, json};

use super::auth::GmailAuth;
use crate::config::Config;
use crate::tools::sanitize_header;

#[derive(Debug, Clone)]
pub struct GmailMessage {
    pub id: String,
    pub thread_id: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub labels: Vec<String>,
    pub snippet: String,
    pub body: String,
    pub is_html: bool,
}

#[derive(Debug, Clone)]
pub struct GmailLabel {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub thread_id: Option<String>,
    pub in_reply_to: Option<String>,
}

pub struct GmailClient {
    http: reqwest::Client,
    auth: Option<GmailAuth>,
    secret_path: PathBuf,
}

impl GmailClient {
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent("google-harness/0.1")
            .build()
            .context("building Gmail HTTP client")?;
        let secret_path = config.client_secret_path();
        let auth = GmailAuth::from_config(config, http.clone())?;
        Ok(Self {
            http,
            auth,
            secret_path,
        })
    }

    #[cfg(test)]
    pub fn disconnected() -> Self {
        Self {
            http: reqwest::Client::builder()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            auth: None,
            secret_path: PathBuf::from("missing"),
        }
    }

    pub async fn search(&self, query: &str, max_results: u32) -> anyhow::Result<Vec<GmailMessage>> {
        let max_results = max_results.clamp(1, 20);
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages?q={}&maxResults={max_results}",
            urlencoding(query)
        );
        let listed = self.request(Method::GET, &url, None).await?;
        let Some(messages) = listed.get("messages").and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for item in messages {
            let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let url = format!(
                "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Cc&metadataHeaders=Subject&metadataHeaders=Date"
            );
            let message = self.request(Method::GET, &url, None).await?;
            out.push(parse_message(&message));
        }
        Ok(out)
    }

    pub async fn get_message(&self, id: &str) -> anyhow::Result<GmailMessage> {
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=full"
        );
        let message = self.request(Method::GET, &url, None).await?;
        let mut parsed = parse_message(&message);
        if parsed.body.is_empty() {
            if let Some((is_html, body)) = self.raw_body(id).await? {
                parsed.is_html = is_html;
                parsed.body = body;
            }
        }
        Ok(parsed)
    }

    pub async fn get_thread(&self, id: &str) -> anyhow::Result<(Vec<GmailMessage>, bool)> {
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/threads/{id}?format=full"
        );
        let thread = self.request(Method::GET, &url, None).await?;
        let messages = thread
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let truncated = messages.len() > 15;
        let parsed = messages
            .into_iter()
            .take(15)
            .map(|message| parse_message(&message))
            .collect();
        Ok((parsed, truncated))
    }

    pub async fn list_labels(&self) -> anyhow::Result<Vec<GmailLabel>> {
        let value = self
            .request(
                Method::GET,
                "https://gmail.googleapis.com/gmail/v1/users/me/labels",
                None,
            )
            .await?;
        let labels = value
            .get("labels")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(labels
            .into_iter()
            .map(|label| GmailLabel {
                id: label.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: label
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                kind: label
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
            })
            .collect())
    }

    pub async fn create_draft(&self, message: &Outgoing) -> anyhow::Result<String> {
        let raw = build_raw_message(message);
        let mut body = json!({"message": {"raw": raw}});
        if let Some(thread_id) = &message.thread_id {
            body["message"]["threadId"] = json!(thread_id);
        }
        let value = self
            .request(
                Method::POST,
                "https://gmail.googleapis.com/gmail/v1/users/me/drafts",
                Some(&body),
            )
            .await?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Ok(format!("draft created: {id}"))
    }

    pub async fn send(&self, message: &Outgoing) -> anyhow::Result<String> {
        let raw = build_raw_message(message);
        let mut body = json!({"raw": raw});
        if let Some(thread_id) = &message.thread_id {
            body["threadId"] = json!(thread_id);
        }
        let value = self
            .request(
                Method::POST,
                "https://gmail.googleapis.com/gmail/v1/users/me/messages/send",
                Some(&body),
            )
            .await?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Ok(format!("message sent: {id}"))
    }

    pub async fn modify_labels(
        &self,
        id: &str,
        add: &[String],
        remove: &[String],
    ) -> anyhow::Result<String> {
        let body = json!({
            "addLabelIds": add,
            "removeLabelIds": remove,
        });
        let value = self
            .request(
                Method::POST,
                &format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}/modify"),
                Some(&body),
            )
            .await?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        Ok(format!("labels updated: {id}"))
    }

    pub async fn trash(&self, id: &str) -> anyhow::Result<String> {
        let value = self
            .request(
                Method::POST,
                &format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}/trash"),
                Some(&json!({})),
            )
            .await?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        Ok(format!("message trashed: {id}"))
    }

    async fn raw_body(&self, id: &str) -> anyhow::Result<Option<(bool, String)>> {
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=raw"
        );
        let value = self.request(Method::GET, &url, None).await?;
        let Some(raw) = value.get("raw").and_then(|v| v.as_str()) else {
            return Ok(None);
        };
        let bytes = decode_gmail_base64(raw)?;
        Ok(parse_rfc822(&bytes))
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let auth = self.auth.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Gmail client secret is missing at {}. Download a Desktop OAuth client and run `google-harness auth login`.",
                self.secret_path.display()
            )
        })?;
        let mut retried = false;
        loop {
            let token = auth.access_token().await?;
            let mut request = self.http.request(method.clone(), url).bearer_auth(token);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await?;
            if response.status().as_u16() == 401 && !retried {
                auth.invalidate().await;
                retried = true;
                continue;
            }
            let status = response.status();
            let text = response.text().await?;
            if text.len() > 2_000_000 {
                anyhow::bail!("Gmail response is too large to load into a local model context");
            }
            if !status.is_success() {
                anyhow::bail!("gmail {status}: {}", clip(&text, 500));
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).context("parsing Gmail response");
        }
    }
}

pub fn build_raw_message(message: &Outgoing) -> String {
    let mut headers = Vec::new();
    headers.push(format!("To: {}", sanitize_header(&message.to)));
    if !message.cc.trim().is_empty() {
        headers.push(format!("Cc: {}", sanitize_header(&message.cc)));
    }
    if !message.bcc.trim().is_empty() {
        headers.push(format!("Bcc: {}", sanitize_header(&message.bcc)));
    }
    headers.push(format!("Subject: {}", encode_subject(&sanitize_header(&message.subject))));
    if let Some(reply) = &message.in_reply_to {
        let reply = sanitize_header(reply);
        if !reply.is_empty() {
            headers.push(format!("In-Reply-To: {reply}"));
            headers.push(format!("References: {reply}"));
        }
    }
    headers.push("MIME-Version: 1.0".into());
    headers.push("Content-Type: text/plain; charset=utf-8".into());
    headers.push("Content-Transfer-Encoding: 8bit".into());
    let raw = format!("{}\r\n\r\n{}", headers.join("\r\n"), message.body.replace('\0', ""));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

fn encode_subject(subject: &str) -> String {
    if subject.is_ascii() {
        subject.to_string()
    } else {
        format!(
            "=?utf-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(subject.as_bytes())
        )
    }
}

fn parse_message(value: &Value) -> GmailMessage {
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    let mut plain = None;
    let mut html = None;
    walk_parts(&payload, &mut plain, &mut html);
    let (body, is_html) = match (plain, html) {
        (Some(text), _) => (text, false),
        (None, Some(text)) => (text, true),
        (None, None) => (String::new(), false),
    };
    let labels = value
        .get("labelIds")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    GmailMessage {
        id: value.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
        thread_id: value
            .get("threadId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        from: header(&payload, "From"),
        to: header(&payload, "To"),
        cc: header(&payload, "Cc"),
        subject: header(&payload, "Subject"),
        date: header(&payload, "Date"),
        labels,
        snippet: value
            .get("snippet")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        body,
        is_html,
    }
}

fn walk_parts(part: &Value, plain: &mut Option<String>, html: &mut Option<String>) {
    let mime = part.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(data) = part.pointer("/body/data").and_then(|v| v.as_str()) {
        if let Ok(text) = decode_gmail_base64(data).and_then(|bytes| {
            String::from_utf8(bytes).map_err(|err| anyhow::anyhow!(err))
        }) {
            if mime == "text/plain" && plain.is_none() {
                *plain = Some(text);
            } else if mime == "text/html" && html.is_none() {
                *html = Some(text);
            }
        }
    }
    if let Some(parts) = part.get("parts").and_then(|v| v.as_array()) {
        for child in parts {
            walk_parts(child, plain, html);
        }
    }
}

fn header(payload: &Value, name: &str) -> String {
    payload
        .get("headers")
        .and_then(|v| v.as_array())
        .and_then(|headers| {
            headers.iter().find(|header| {
                header
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|candidate| candidate.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
        })
        .and_then(|header| header.get("value").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

fn parse_rfc822(bytes: &[u8]) -> Option<(bool, String)> {
    let message = mail_parser::MessageParser::default()
        .with_mime_headers()
        .parse(bytes)?;
    if let Some(text) = message.body_text(0) {
        return Some((false, text.to_string()));
    }
    if let Some(html) = message.body_html(0) {
        return Some((true, html.to_string()));
    }
    None
}

pub fn decode_gmail_base64(data: &str) -> anyhow::Result<Vec<u8>> {
    let cleaned: String = data.chars().filter(|ch| !ch.is_whitespace()).collect();
    let mut padded = cleaned.clone();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    base64::engine::general_purpose::URL_SAFE
        .decode(padded)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(cleaned))
        .context("decoding Gmail base64")
}

fn urlencoding(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_header_injection_from_raw_message() {
        let raw = build_raw_message(&Outgoing {
            to: "a@b.com\r\nBcc: evil@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello\nSubject: overwritten".into(),
            body: "Body stays here".into(),
            thread_id: None,
            in_reply_to: None,
        });
        let bytes = decode_gmail_base64(&raw).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let header = text.split("\r\n\r\n").next().unwrap();
        let header_lines: Vec<&str> = header.split("\r\n").collect();
        assert!(header_lines.iter().all(|line| !line.to_ascii_lowercase().starts_with("bcc:")));
        assert!(header_lines.iter().any(|line| line.contains("a@b.com")));
        assert_eq!(
            header_lines.iter().filter(|line| line.to_ascii_lowercase().starts_with("subject:")).count(),
            1
        );
        assert!(text.contains("Body stays here"));
    }
}
