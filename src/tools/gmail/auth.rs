use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use base64::Engine;
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use crate::config::{Config, config_dir};

pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const SCOPE_COMPOSE: &str = "https://www.googleapis.com/auth/gmail.compose";
pub const SCOPE_MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";

const KEYRING_SERVICE: &str = "google-harness";
const KEYRING_ACCOUNT: &str = "gmail-refresh-token";

#[derive(Debug, Deserialize)]
struct SecretFile {
    installed: Option<ClientSecret>,
    web: Option<ClientSecret>,
}

#[derive(Debug, Deserialize)]
struct ClientSecret {
    client_id: String,
    client_secret: String,
    auth_uri: Option<String>,
    token_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

struct AuthState {
    refresh_token: Option<String>,
    access_token: Option<String>,
    expires_at: Option<Instant>,
}

pub struct GmailAuth {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token_uri: String,
    state: Mutex<AuthState>,
}

impl GmailAuth {
    pub fn from_config(config: &Config, http: reqwest::Client) -> anyhow::Result<Option<Self>> {
        let path = config.client_secret_path();
        if !path.exists() {
            return Ok(None);
        }
        let secret = load_client_secret(&path)?;
        let refresh = stored_refresh_token().ok().flatten();
        Ok(Some(Self {
            http,
            client_id: secret.client_id,
            client_secret: secret.client_secret,
            token_uri: secret
                .token_uri
                .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into()),
            state: Mutex::new(AuthState {
                refresh_token: refresh,
                access_token: None,
                expires_at: None,
            }),
        }))
    }

    pub async fn access_token(&self) -> anyhow::Result<String> {
        {
            let state = self.state.lock().await;
            if let (Some(token), Some(expiry)) = (&state.access_token, state.expires_at) {
                if expiry > Instant::now() + Duration::from_secs(60) {
                    return Ok(token.clone());
                }
            }
        }
        let refresh = self
            .state
            .lock()
            .await
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Gmail is not authenticated. Run `google-harness auth login`."))?;
        let token = request_token(
            &self.http,
            &self.token_uri,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ],
        )
        .await?;
        let mut state = self.state.lock().await;
        if let Some(rotated) = token.refresh_token.clone() {
            state.refresh_token = Some(rotated.clone());
            save_refresh_token(&rotated)?;
        }
        state.access_token = Some(token.access_token.clone());
        state.expires_at = Some(Instant::now() + Duration::from_secs(token.expires_in.unwrap_or(3600)));
        Ok(token.access_token)
    }

    pub async fn invalidate(&self) {
        let mut state = self.state.lock().await;
        state.access_token = None;
        state.expires_at = None;
    }
}

pub async fn login(config: &Config) -> anyhow::Result<()> {
    let secret_path = config.client_secret_path();
    let secret = load_client_secret(&secret_path)?;
    let redirect = config.redirect_uri();
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let csrf = CsrfToken::new_random();
    let state = csrf.secret().clone();
    let auth_uri = secret
        .auth_uri
        .unwrap_or_else(|| "https://accounts.google.com/o/oauth2/v2/auth".into());
    let mut url = Url::parse(&auth_uri).context("invalid auth URI")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", &secret.client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_type", "code")
            .append_pair(
                "scope",
                &format!("{SCOPE_READONLY} {SCOPE_COMPOSE} {SCOPE_MODIFY}"),
            )
            .append_pair("code_challenge", challenge.as_str())
            .append_pair("code_challenge_method", challenge.method().as_str())
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("state", &state);
    }

    let addr = format!("127.0.0.1:{}", config.gmail.redirect_port);
    let server = tiny_http::Server::http(&addr).map_err(|err| {
        anyhow::anyhow!(
            "could not listen on {addr}: {err}. Set gmail.redirect_port and register the matching redirect URI."
        )
    })?;
    println!("Open this URL if the browser does not appear:\n{url}\n");
    println!("Waiting for the OAuth redirect on {redirect}");
    open_browser(url.as_str());

    let expected_state = state.clone();
    let (code, returned_state) = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        let request = server
            .recv_timeout(Duration::from_secs(180))
            .map_err(|err| anyhow::anyhow!("waiting for OAuth redirect: {err}"))?
            .context("timed out waiting for the Google login redirect")?;
        let request_url = request.url().to_string();
        let response = tiny_http::Response::from_string(
            "google-harness login complete. You can close this tab.",
        );
        let _ = request.respond(response);
        parse_redirect(&request_url)
    })
    .await??;

    if returned_state != expected_state {
        anyhow::bail!("OAuth state did not match. Run auth login again.");
    }
    let token_uri = secret
        .token_uri
        .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let token = request_token(
        &http,
        &token_uri,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", secret.client_id.as_str()),
            ("client_secret", secret.client_secret.as_str()),
            ("code_verifier", verifier.secret().as_str()),
        ],
    )
    .await?;
    let refresh = token
        .refresh_token
        .ok_or_else(|| anyhow::anyhow!("Google did not return a refresh token. Remove the app's access in your Google account and run auth login again."))?;
    save_refresh_token(&refresh)?;
    println!("Gmail login saved.");
    Ok(())
}

pub fn status() -> anyhow::Result<String> {
    match load_refresh_token()? {
        Some(Location::Keyring) => Ok("Gmail refresh token is stored in the OS keychain.".into()),
        Some(Location::File) => {
            Ok("Gmail refresh token is stored in the permission-restricted fallback file.".into())
        }
        None => Ok("Gmail is not authenticated. Run `google-harness auth login`.".into()),
    }
}

pub fn logout() -> anyhow::Result<()> {
    let _ = delete_keyring();
    let path = fallback_token_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("Removed stored Gmail credentials.");
    Ok(())
}

enum Location {
    Keyring,
    File,
}

fn load_client_secret(path: &Path) -> anyhow::Result<ClientSecret> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading Google client secret at {}. Download a Desktop OAuth client JSON and place it there.",
            path.display()
        )
    })?;
    let parsed: SecretFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    parsed
        .installed
        .or(parsed.web)
        .context("client secret JSON needs an `installed` or `web` client")
}

fn parse_redirect(request_url: &str) -> anyhow::Result<(String, String)> {
    let absolute = if request_url.starts_with("http") {
        request_url.to_string()
    } else {
        format!("http://127.0.0.1{request_url}")
    };
    let parsed = Url::parse(&absolute)?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        anyhow::bail!("Google returned an OAuth error: {error}");
    }
    Ok((
        code.context("OAuth redirect did not include a code")?,
        state.unwrap_or_default(),
    ))
}

async fn request_token(
    http: &reqwest::Client,
    token_uri: &str,
    form: &[(&str, &str)],
) -> anyhow::Result<TokenResponse> {
    let response = http.post(token_uri).form(form).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("token endpoint returned {status}: {text}");
    }
    serde_json::from_str(&text).context("parsing token response")
}

fn load_refresh_token() -> anyhow::Result<Option<Location>> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        if entry.get_password().is_ok() {
            return Ok(Some(Location::Keyring));
        }
    }
    let path = fallback_token_path();
    if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        if !text.trim().is_empty() {
            return Ok(Some(Location::File));
        }
    }
    Ok(None)
}

fn read_refresh_token() -> anyhow::Result<Option<String>> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        if let Ok(token) = entry.get_password() {
            if !token.trim().is_empty() {
                return Ok(Some(token));
            }
        }
    }
    let path = fallback_token_path();
    if path.exists() {
        let text = std::fs::read_to_string(path)?;
        let token = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(text.trim())?)?;
        if !token.trim().is_empty() {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

fn save_refresh_token(token: &str) -> anyhow::Result<()> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        if entry.set_password(token).is_ok() {
            let path = fallback_token_path();
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            return Ok(());
        }
    }
    write_fallback(token)?;
    tracing::warn!("OS keychain was unavailable; stored the Gmail refresh token in a mode 0600 file");
    Ok(())
}

fn delete_keyring() -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
    entry.delete_credential()?;
    Ok(())
}

fn fallback_token_path() -> std::path::PathBuf {
    config_dir().join("refresh_token")
}

fn write_fallback(token: &str) -> anyhow::Result<()> {
    let path = fallback_token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        use std::io::Write;
        file.write_all(encoded.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, encoded)?;
    }
    Ok(())
}

fn open_browser(url: &str) {
    let result = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).spawn()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(url).spawn()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = url;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "open the URL manually",
            ))
        }
    };
    if let Err(err) = result {
        eprintln!("Could not open a browser ({err}). Use the URL above.");
    }
}

pub fn stored_refresh_token() -> anyhow::Result<Option<String>> {
    read_refresh_token()
}
