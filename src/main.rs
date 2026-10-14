mod agent;
mod config;
mod llm;
mod policy;
mod storage;
mod tools;
mod tui;
mod util;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::events::AgentEvent;
use crate::agent::{spawn_persist, AgentRuntime, RuntimeDeps};
use crate::config::Config;
use crate::llm::{LlmProvider, OpenAiCompatClient};
use crate::storage::Store;
use crate::tools::gmail::{self, GmailClient};
use crate::tools::ToolRegistry;

#[derive(Debug, Parser)]
#[command(
    name = "google-harness",
    version,
    about = "Local Gmail agent harness with a live agent view and approval for every write."
)]
struct Cli {
    /// Path to a TOML config file. Defaults to ~/.config/google-harness/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the interactive agent dashboard. This is the default.
    Tui,
    /// Run one goal in the terminal without the full-screen UI.
    Run {
        /// Goal for the supervisor. Separate words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        goal: Vec<String>,
    },
    /// Sign in to Gmail, check the stored token, or remove it.
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },
    /// List models exposed by the configured local or OpenAI-compatible server.
    Models,
    /// Write or print the harness config.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Print a saved session trace. Omit the id to list recent sessions.
    Replay {
        /// Session id, or "latest".
        session_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Run the browser OAuth PKCE login and store the refresh token.
    Login,
    /// Say whether a Gmail refresh token is stored.
    Status,
    /// Delete the stored Gmail refresh token.
    Logout,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Write the example config to ~/.config/google-harness/config.toml.
    Init,
    /// Print the config path and the loaded settings.
    Show,
}

struct Services {
    config: Config,
    llm: Arc<dyn LlmProvider>,
    store: Arc<Store>,
    tools: Arc<ToolRegistry>,
}

impl Services {
    fn boot(config: Config) -> anyhow::Result<Self> {
        if let Err(err) = config::init_tracing(&config.log_path()) {
            eprintln!("logging disabled: {err}");
        }
        let store = Arc::new(Store::open(&config.sqlite_path())?);
        let llm = Arc::new(OpenAiCompatClient::new(&config.llm)?);
        let gmail = Arc::new(GmailClient::from_config(&config)?);
        let tools = Arc::new(gmail::all_tools(gmail));
        Ok(Self {
            config,
            llm,
            store,
            tools,
        })
    }

    fn deps(
        &self,
        events: broadcast::Sender<AgentEvent>,
        approvals: mpsc::UnboundedSender<crate::agent::events::ApprovalTicket>,
        session_id: String,
        cancel: CancellationToken,
    ) -> RuntimeDeps {
        RuntimeDeps {
            config: self.config.clone(),
            llm: Arc::clone(&self.llm),
            tools: Arc::clone(&self.tools),
            events,
            approvals,
            store: Arc::clone(&self.store),
            session_id,
            cancel,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Config { action: ConfigCommand::Init } => {
            let path = config::write_example_config()?;
            println!("Wrote {}", path.display());
            return Ok(());
        }
        Command::Config { action: ConfigCommand::Show } => {
            let loaded = Config::load(cli.config.as_deref())?;
            println!("config: {}", cli.config.unwrap_or_else(config::default_config_path).display());
            println!("llm.base_url: {}", loaded.llm.base_url);
            println!("llm.api_style: {}", loaded.llm.api_style);
            println!("llm.orchestrator_model: {}", loaded.llm.orchestrator_model);
            println!("llm.worker_model: {}", loaded.llm.worker_model);
            println!("llm.max_context_tokens: {}", loaded.llm.max_context_tokens);
            println!("gmail.client_secret: {}", loaded.client_secret_path().display());
            println!("gmail.redirect_uri: {}", loaded.redirect_uri());
            println!("storage.sqlite: {}", loaded.sqlite_path().display());
            println!("storage.log: {}", loaded.log_path().display());
            return Ok(());
        }
        Command::Auth { action: AuthCommand::Status } => {
            println!("{}", gmail::status()?);
            return Ok(());
        }
        Command::Auth { action: AuthCommand::Logout } => {
            gmail::logout()?;
            return Ok(());
        }
        command => {
            let loaded = Config::load(cli.config.as_deref())?;
            match command {
                Command::Auth { action: AuthCommand::Login } => gmail::login(&loaded).await?,
                Command::Models => {
                    let services = Services::boot(loaded)?;
                    let models = services.llm.list_models().await?;
                    if models.is_empty() {
                        println!("The server returned no models.");
                    }
                    for model in models {
                        println!("{model}");
                    }
                }
                Command::Replay { session_id } => {
                    let services = Services::boot(loaded)?;
                    replay(&services.store, session_id.as_deref())?;
                }
                Command::Run { goal } => {
                    let services = Services::boot(loaded)?;
                    run_headless(services, goal.join(" ")).await?;
                }
                Command::Tui => {
                    let services = Services::boot(loaded)?;
                    run_tui(services).await?;
                }
                Command::Config { .. } | Command::Auth { .. } => unreachable!(),
            }
        }
    }
    Ok(())
}

async fn run_tui(services: Services) -> anyhow::Result<()> {
    let session_id = Uuid::new_v4().to_string();
    services.store.create_session(&session_id, "")?;
    let (events, initial) = broadcast::channel(4096);
    drop(initial);
    let persist_rx = events.subscribe();
    let ui_rx = events.subscribe();
    spawn_persist(Arc::clone(&services.store), session_id.clone(), persist_rx);
    let (approvals, approval_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let model = services.config.llm.orchestrator_model.clone();
    let deps = services.deps(events, approvals, session_id, cancel);
    tui::run_with_approvals(deps, model, approval_rx, ui_rx).await
}

async fn run_headless(services: Services, goal: String) -> anyhow::Result<()> {
    let session_id = Uuid::new_v4().to_string();
    services.store.create_session(&session_id, &goal)?;
    println!("session {session_id}");
    let (events, initial) = broadcast::channel(4096);
    drop(initial);
    let persist_rx = events.subscribe();
    let mut ui_rx = events.subscribe();
    spawn_persist(Arc::clone(&services.store), session_id.clone(), persist_rx);
    let (approvals, mut approval_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let deps = services.deps(events, approvals, session_id, cancel);
    let runtime = AgentRuntime::supervisor(deps);
    let mut task = tokio::spawn(async move { runtime.run(goal).await });
    let mut at_eol = true;
    loop {
        tokio::select! {
            result = &mut task => {
                if !at_eol {
                    println!();
                }
                return match result {
                    Ok(Ok(text)) => {
                        println!("{text}");
                        Ok(())
                    }
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(anyhow::anyhow!("agent task failed: {err}")),
                };
            }
            event = ui_rx.recv() => {
                match event {
                    Ok(event) => {
                        if matches!(event, AgentEvent::ThinkingDelta { .. }) {
                            if let AgentEvent::ThinkingDelta { delta, .. } = event {
                                print!("{delta}");
                                let _ = std::io::stdout().flush();
                                at_eol = false;
                            }
                        } else if let Some(line) = format_event_line(&event) {
                            if !at_eol {
                                println!();
                            }
                            println!("{line}");
                            at_eol = true;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !at_eol {
                            println!();
                        }
                        println!("(dropped some trace events)");
                        at_eol = true;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            ticket = approval_rx.recv() => {
                match ticket {
                    Some(ticket) => {
                        if !at_eol {
                            println!();
                            at_eol = true;
                        }
                        let decision = prompt_approval(&ticket).await;
                        let _ = ticket.respond(decision);
                    }
                    None => break,
                }
            }
        }
    }
    if !at_eol {
        println!();
    }
    match task.await {
        Ok(Ok(text)) => {
            println!("{text}");
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow::anyhow!("agent task failed: {err}")),
    }
}

async fn prompt_approval(ticket: &crate::agent::events::ApprovalTicket) -> crate::policy::ApprovalDecision {
    use crate::policy::ApprovalDecision;
    if !std::io::stdin().is_terminal() {
        return ApprovalDecision::Reject {
            reason: "non-interactive session cannot approve writes".into(),
        };
    }
    println!("\n=== approval required ===");
    println!("agent: {}", ticket.agent_id);
    println!("tool: {}", ticket.tool_name);
    println!("{}", ticket.preview);
    println!("[y] approve   [n] reject   [m] replace arguments with one line of JSON");
    let line = read_line().await;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Approve,
        "m" => {
            println!("Paste JSON arguments on one line:");
            let json_line = read_line().await;
            match serde_json::from_str(json_line.trim()) {
                Ok(value) => ApprovalDecision::ApproveWithModifiedArgs(value),
                Err(err) => ApprovalDecision::Reject {
                    reason: format!("invalid JSON: {err}"),
                },
            }
        }
        "n" | "no" | "" => ApprovalDecision::Reject {
            reason: "rejected by user".into(),
        },
        other => ApprovalDecision::Reject {
            reason: other.to_string(),
        },
    }
}

async fn read_line() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .unwrap_or_default()
}

fn format_event_line(event: &AgentEvent) -> Option<String> {
    let value = event.to_json();
    let summary = value
        .get("summary")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    if summary.is_empty() {
        return None;
    }
    Some(format!("[{}] {summary}", event.agent_id()))
}

fn replay(store: &Store, session_id: Option<&str>) -> anyhow::Result<()> {
    let Some(session_id) = session_id else {
        let sessions = store.recent_sessions(20)?;
        if sessions.is_empty() {
            println!("No saved sessions.");
            return Ok(());
        }
        for session in sessions {
            println!("{}  {}  {}", session.created_at, session.id, session.goal);
        }
        return Ok(());
    };
    let session_id = if session_id == "latest" {
        store
            .latest_session_id()?
            .context("no saved sessions")?
    } else {
        session_id.to_string()
    };
    let session = store
        .session(&session_id)?
        .with_context(|| format!("unknown session {session_id}"))?;
    println!("session {}  {}", session.id, session.created_at);
    println!("goal: {}", session.goal);
    for event in store.events_for(&session.id)? {
        let summary = serde_json::from_str::<serde_json::Value>(&event.payload)
            .ok()
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(|item| item.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| event.payload.clone());
        println!(
            "[{}] {} {} {}",
            event.created_at,
            event.agent_id,
            event.kind,
            summary
        );
    }
    Ok(())
}
