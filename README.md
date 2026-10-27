# agentic-cli

A local-first Gmail agent harness. A supervisor and its workers call a local OpenAI-compatible model (Ollama, llama.cpp, vLLM, or LM Studio). Read tools run on their own. Every draft, send, reply, label change, and trash stops for approval in the terminal UI.

## Prerequisites

- Rust 1.80 or newer (`rustup` and `cargo`). The first build compiles the bundled SQLite library, so the Xcode command-line tools (macOS) or a C compiler (Linux) must be installed.
- macOS uses the system keychain and does not need extra packages. On Linux, install `pkg-config` and `libdbus-1-dev` so the keyring crate can reach Secret Service.
- A local model server. Ollama is the default. Any server that speaks `POST /v1/chat/completions` with tool calls also works.

## Compile

```bash
cd /Users/anupam/repos/agentic-cli

cargo build
cargo test
cargo build --release

# optional: install the binary to ~/.cargo/bin
cargo install --path .
```

The release binary is `target/release/agentic-cli`.

## OpenRouter

The default host is OpenRouter. Put your key in `~/.config/agentic-cli/config.toml` (this file is not in git):

```toml
[llm]
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-..."
api_style = "openai"
orchestrator_model = "recursive/flint-v1.0:free"
worker_model = "recursive/flint-v1.0:free"
```

`api_style` must be `openai`. The base URL must end in `/v1`. Use an OpenRouter model id, and pick one that accepts tool calls. `agentic-cli config show` prints the URL and model names and does not print `api_key`.

```bash
agentic-cli models
```

## Local model

Ollama still works if you point the same fields at it:

```bash
ollama serve
ollama pull qwen2.5:14b
```

```toml
[llm]
base_url = "http://localhost:11434/v1"
api_key = "ollama"
api_style = "ollama"
orchestrator_model = "qwen2.5:14b"
worker_model = "qwen2.5:14b"
```

`api_style = "auto"` tries `/v1/chat/completions` first, then Ollama's native `/api/chat` when that route returns 404.

## Gmail OAuth

1. Create a Google Cloud project and enable the Gmail API.
2. Configure the OAuth consent screen. In testing mode, add the Gmail account you will sign in with.
3. Add these scopes:
   - `https://www.googleapis.com/auth/gmail.readonly`
   - `https://www.googleapis.com/auth/gmail.compose`
   - `https://www.googleapis.com/auth/gmail.modify`
4. Create an OAuth client of type **Desktop app**. Add this authorized redirect URI:

   `http://127.0.0.1:8765/callback`

5. Download the client JSON to `~/.config/agentic-cli/client_secret.json`.
6. Sign in:

```bash
cargo run --release -- auth login
```

The refresh token is stored in the macOS keychain (or Linux Secret Service). If the keychain is unavailable, it falls back to `~/.config/agentic-cli/refresh_token` with mode `0600`.

```bash
agentic-cli auth status
agentic-cli auth logout
```

## Configuration

```bash
agentic-cli config init
agentic-cli config show
```

`config init` writes [`config.example.toml`](config.example.toml) to `~/.config/agentic-cli/config.toml`. The fields that matter first are `llm.base_url`, `llm.orchestrator_model`, `llm.worker_model`, `llm.max_context_tokens`, and `storage.sqlite_path`. Missing files use the same defaults as the example.

## Run

```bash
cargo run --release -- tui
# or, after install:
agentic-cli
```

Type a goal in the bottom bar and press Enter. The left pane lists the supervisor and any workers, with status and token counts. The right pane streams thoughts and tool calls for the selected agent.

Headless one-shot, still with a prompt before any write:

```bash
agentic-cli run summarize unread mail from today
```

Saved traces:

```bash
agentic-cli replay
agentic-cli replay latest
agentic-cli replay <session-id>
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
