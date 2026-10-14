# google-harness

A local-first Gmail agent harness. A supervisor and its workers call a local OpenAI-compatible model (Ollama, llama.cpp, vLLM, or LM Studio). Read tools run on their own. Every draft, send, reply, label change, and trash stops for approval in the terminal UI.

## Prerequisites

- Rust 1.80 or newer (`rustup` and `cargo`). The first build compiles the bundled SQLite library, so the Xcode command-line tools (macOS) or a C compiler (Linux) must be installed.
- macOS uses the system keychain and does not need extra packages. On Linux, install `pkg-config` and `libdbus-1-dev` so the keyring crate can reach Secret Service.
- A local model server. Ollama is the default. Any server that speaks `POST /v1/chat/completions` with tool calls also works.

## Compile

```bash
cd /Users/anupam/repos/google-harness

cargo build
cargo test
cargo build --release

# optional: install the binary to ~/.cargo/bin
cargo install --path .
```

The release binary is `target/release/google-harness`.

## Local model

```bash
ollama serve
ollama pull qwen2.5:14b
# or: ollama pull llama3.1:8b
```

Ollama exposes `http://localhost:11434/v1`. The harness uses that OpenAI-compatible endpoint first. If the server answers 404, it retries Ollama's native `/api/chat`. Set `llm.api_style` to `openai` or `ollama` to force one protocol. Point `llm.base_url` and `llm.api_key` at a cloud endpoint when you want a remote fallback.

```bash
google-harness models
```

## Gmail OAuth

1. Create a Google Cloud project and enable the Gmail API.
2. Configure the OAuth consent screen. In testing mode, add the Gmail account you will sign in with.
3. Add these scopes:
   - `https://www.googleapis.com/auth/gmail.readonly`
   - `https://www.googleapis.com/auth/gmail.compose`
   - `https://www.googleapis.com/auth/gmail.modify`
4. Create an OAuth client of type **Desktop app**. Add this authorized redirect URI:

   `http://127.0.0.1:8765/callback`

5. Download the client JSON to `~/.config/google-harness/client_secret.json`.
6. Sign in:

```bash
cargo run --release -- auth login
```

The refresh token is stored in the macOS keychain (or Linux Secret Service). If the keychain is unavailable, it falls back to `~/.config/google-harness/refresh_token` with mode `0600`.

```bash
google-harness auth status
google-harness auth logout
```

## Configuration

```bash
google-harness config init
google-harness config show
```

`config init` writes [`config.example.toml`](config.example.toml) to `~/.config/google-harness/config.toml`. The fields that matter first are `llm.base_url`, `llm.orchestrator_model`, `llm.worker_model`, `llm.max_context_tokens`, and `storage.sqlite_path`. Missing files use the same defaults as the example.

## Run

```bash
cargo run --release -- tui
# or, after install:
google-harness
```

Type a goal in the bottom bar and press Enter. The left pane lists the supervisor and any workers, with status and token counts. The right pane streams thoughts and tool calls for the selected agent.

Headless one-shot, still with a prompt before any write:

```bash
google-harness run summarize unread mail from today
```

Saved traces:

```bash
google-harness replay
google-harness replay latest
google-harness replay <session-id>
```

The session id is shown in the TUI startup line and at the start of a headless run. SQLite also stores an idempotency key for each successful write so the same approved payload is not sent twice.

## Keybindings

| Key | Action |
| --- | --- |
| Tab | Move focus between the agent list, the trace, and the goal box |
| Enter | Run the goal in the goal box |
| Up / Down | Select an agent, or scroll the trace |
| PgUp / PgDn | Scroll the trace |
| y | Approve the pending write |
| e | Edit the write arguments as JSON |
| Ctrl-S | Save the edited JSON and approve it |
| n | Reject the write and type a reason. Enter sends the reason back to the agent |
| Esc | Leave edit or reject mode |
| q | Quit when focus is not in the goal box |
| Ctrl-C | Quit |

While a write is waiting, other read-only work can keep running. Closing the UI rejects anything still waiting.

## Safety model

- `gmail_search`, `gmail_get_message`, `gmail_get_thread`, and `gmail_list_labels` are autonomous.
- `gmail_create_draft`, `gmail_send_message`, `gmail_reply_thread`, `gmail_modify_labels`, and `gmail_trash_message` always wait for approval. The model cannot skip that gate.
- Message bodies are converted from HTML to Markdown, stripped of scripts, styles, and images, and wrapped in `<untrusted_email_content>`. Quoted reply chains are cut unless a tool asks to keep them.
- A bad tool-call payload is sent back to the model with the JSON schema, up to two retries.
- Workers read mail in their own context so a long thread does not fill the supervisor window.
- A non-interactive `run` rejects writes instead of sending them.
