//! OAuth2 installed-app flow and Service Account JWT flow for Google Drive.
//!
//! ## OAuth2 (desktop / installed-app)
//!
//! Tokens are persisted at `~/.config/remotefs/gdrive_token_<email>.json`,
//! keyed by the authenticated account's email address. Multiple accounts are
//! supported simultaneously — each gets its own token file.
//! Expired access tokens are refreshed automatically using the stored refresh token.
//!
//! ## Service Accounts
//!
//! Tokens are NOT persisted. A fresh JWT assertion is signed on every mount,
//! and again whenever the access token expires (typically after 1 hour).
//! JWT signing is a local operation and is cheap.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
const TOKEN_DIR: &str = ".config/remotefs";

// ---------------------------------------------------------------------------
// OAuth2 credentials file (from Google Cloud Console)
// ---------------------------------------------------------------------------

/// Top-level structure of a `credentials.json` file downloaded from the
/// Google Cloud Console for an "installed app" OAuth2 client.
#[derive(Deserialize)]
pub struct CredentialsFile {
    pub installed: OAuthClient,
}

#[derive(Deserialize, Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
    pub auth_uri: String,
    pub token_uri: String,
}

// ---------------------------------------------------------------------------
// Service Account key file (from Google Cloud Console)
// ---------------------------------------------------------------------------

/// Structure of a service account JSON key file downloaded from the
/// Google Cloud Console (IAM → Service Accounts → Keys).
#[derive(Deserialize, Clone)]
pub struct ServiceAccountKey {
    pub client_email: String,
    pub private_key: String,
    pub token_uri: String,
}

/// Returns `true` if the JSON text is a service account key file
/// (`"type": "service_account"`), as opposed to an OAuth2 credentials file.
pub fn is_service_account(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|v| v["type"].as_str() == Some("service_account"))
        .unwrap_or(false)
}

/// Obtain a short-lived access token for a service account via JWT assertion.
///
/// No token file is written — call this again when the token expires.
pub fn obtain_service_account_token(key: &ServiceAccountKey, http: &ureq::Agent) -> Result<Token> {
    let jwt = make_service_account_jwt(key)?;
    let params = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", jwt.as_str()),
    ];
    let resp: TokenResponse = http
        .post(&key.token_uri)
        .send_form(&params)
        .map_err(|e| anyhow!("service account token request: {e}"))?
        .into_json()
        .context("parse service account token response")?;
    Ok(Token {
        access_token: resp.access_token,
        refresh_token: None,
        expires_at: now_secs() + resp.expires_in.unwrap_or(3600),
        token_uri: key.token_uri.clone(),
        client_id: String::new(),
        client_secret: String::new(),
        email: key.client_email.clone(),
    })
}

fn make_service_account_jwt(key: &ServiceAccountKey) -> Result<String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        scope: &'a str,
        aud: &'a str,
        iat: u64,
        exp: u64,
    }

    let now = now_secs();
    let claims = Claims {
        iss: &key.client_email,
        scope: DRIVE_SCOPE,
        aud: &key.token_uri,
        iat: now,
        exp: now + 3600,
    };
    let encoding_key = EncodingKey::from_rsa_pem(key.private_key.as_bytes())
        .map_err(|e| anyhow!("parse service account private key: {e}"))?;
    encode(&Header::new(Algorithm::RS256), &claims, &encoding_key)
        .map_err(|e| anyhow!("encode service account JWT: {e}"))
}

// ---------------------------------------------------------------------------
// OAuth2 token
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) at which the access token expires.
    pub expires_at: u64,
    pub token_uri: String,
    pub client_id: String,
    pub client_secret: String,
    /// Authenticated account email; used to key the token file on disk.
    /// Empty string means unknown (legacy tokens written before this field existed).
    #[serde(default)]
    pub email: String,
}

impl Token {
    pub fn is_expired(&self) -> bool {
        let now = now_secs();
        now + 60 >= self.expires_at // refresh 60 s early
    }

    /// Exchange the refresh token for a new access token in-place.
    pub fn refresh(&mut self, http: &ureq::Agent) -> Result<()> {
        let refresh_token = self
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("no refresh token available; re-authorize with gdrive"))?;

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];

        let resp: TokenResponse = http
            .post(&self.token_uri)
            .send_form(&params)
            .map_err(|e| anyhow!("token refresh request: {e}"))?
            .into_json()
            .context("parse token refresh")?;

        self.access_token = resp.access_token;
        if let Some(rt) = resp.refresh_token {
            self.refresh_token = Some(rt);
        }
        self.expires_at = now_secs() + resp.expires_in.unwrap_or(3600);
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

// ---------------------------------------------------------------------------
// OAuth2 public entry point
// ---------------------------------------------------------------------------

/// Load a cached token or run the full browser-based authorization flow.
pub fn load_or_authorize(client: &OAuthClient, http: &ureq::Agent, email: &str) -> Result<Token> {
    let path = token_path(email);

    if path.exists() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut token) = serde_json::from_str::<Token>(&text) {
                if token.is_expired() {
                    match token.refresh(http) {
                        Ok(()) => {
                            save_token(&token)?;
                            return Ok(token);
                        }
                        Err(e) => {
                            // Refresh token revoked or rejected — fall through to
                            // re-authorize via browser.
                            eprintln!("Stored token refresh failed ({e:#}); re-authorizing…");
                        }
                    }
                } else {
                    return Ok(token);
                }
            }
        }
    }

    let token = run_browser_flow(client, http)?;
    save_token(&token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Browser OAuth2 flow
// ---------------------------------------------------------------------------

fn run_browser_flow(client: &OAuthClient, http: &ureq::Agent) -> Result<Token> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind local OAuth2 redirect server")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}");

    let auth_url = build_auth_url(&client.auth_uri, &client.client_id, &redirect_uri)?;

    eprintln!("\nOpening browser for Google Drive authorization…");
    eprintln!("If the browser does not open automatically, visit:\n  {auth_url}\n");
    open_browser(&auth_url);

    let (mut stream, _) = listener.accept().context("accept OAuth2 redirect")?;
    let code = extract_code(&stream)?;

    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
          <html><body><h2>Authorized!</h2><p>You can close this tab.</p></body></html>",
    );

    exchange_code(client, http, &code, &redirect_uri)
}

fn build_auth_url(auth_uri: &str, client_id: &str, redirect_uri: &str) -> Result<String> {
    let mut url = url::Url::parse(auth_uri).context("parse auth_uri")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", DRIVE_SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

fn extract_code(stream: &std::net::TcpStream) -> Result<String> {
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;

    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed HTTP request line: {first_line:?}"))?;

    for param in path
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
    {
        if let Some(code) = param.strip_prefix("code=") {
            return Ok(code.to_string());
        }
    }

    if path.contains("error=access_denied") {
        return Err(anyhow!("Google Drive authorization was denied by the user"));
    }
    Err(anyhow!("no authorization code in redirect: {path}"))
}

fn exchange_code(
    client: &OAuthClient,
    http: &ureq::Agent,
    code: &str,
    redirect_uri: &str,
) -> Result<Token> {
    let resp: TokenResponse = http
        .post(&client.token_uri)
        .send_form(&[
            ("code", code),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .map_err(|e| anyhow!("exchange code request: {e}"))?
        .into_json()
        .context("parse code exchange")?;

    Ok(Token {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at: now_secs() + resp.expires_in.unwrap_or(3600),
        token_uri: client.token_uri.clone(),
        client_id: client.client_id.clone(),
        client_secret: client.client_secret.clone(),
        email: String::new(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn token_path(email: &str) -> PathBuf {
    let suffix = if email.is_empty() {
        "default".to_string()
    } else {
        email.replace(['@', '.'], "_")
    };
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(TOKEN_DIR)
        .join(format!("gdrive_token_{suffix}.json"))
}

pub(super) fn save_token(token: &Token) -> Result<()> {
    let path = token_path(&token.email);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, json).context("write token file")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("Please open the URL above manually.");
}
