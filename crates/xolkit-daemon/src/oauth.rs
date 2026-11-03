//! Signing in to remote MCP servers with OAuth. The daemon finds a server's authorization
//! server, registers xolkit with it, and catches the browser's redirect on a loopback port; the
//! app only opens the page. Tokens go to the OS keyring, bound to the server's URL, and rmcp
//! refreshes them as they run out.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rmcp::service::ClientInitializeError;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};
use rmcp::transport::streamable_http_client::AuthRequiredError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xolkit_core::{get_secret, oauth_key, set_secret};

/// How long a sign-in waits for the browser to come back.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a browser connection gets to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest request head read from the browser.
const MAX_REQUEST: usize = 16 * 1024;

/// Where tokens are kept: the OS keyring, or memory in tests. Calls may block.
pub(crate) trait Vault: Send + Sync + 'static {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: Option<&str>) -> Result<()>;
}

/// The OS keyring, as for every other secret.
pub(crate) struct Keyring;

impl Vault for Keyring {
    fn get(&self, key: &str) -> Result<Option<String>> {
        get_secret(key)
    }

    fn set(&self, key: &str, value: Option<&str>) -> Result<()> {
        set_secret(key, value)
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    /// The URL the tokens are for.
    url: String,
    credentials: StoredCredentials,
}

/// A server's tokens in the vault, bound to its URL: tokens saved for one address are never sent
/// to another, even when a server keeps its name and changes its URL.
#[derive(Clone)]
pub(crate) struct Tokens {
    vault: Arc<dyn Vault>,
    key: String,
    url: String,
}

impl Tokens {
    pub(crate) fn new(vault: Arc<dyn Vault>, name: &str, url: &str) -> Tokens {
        Tokens {
            vault,
            key: oauth_key(name),
            url: url.to_owned(),
        }
    }

    /// The saved credentials, if they are for this URL. Blocks.
    fn read(&self) -> Result<Option<StoredCredentials>> {
        let Some(text) = self.vault.get(&self.key)? else {
            return Ok(None);
        };
        // Something unreadable is as good as nothing: signing in again replaces it.
        Ok(serde_json::from_str::<Saved>(&text)
            .ok()
            .filter(|saved| saved.url == self.url)
            .map(|saved| saved.credentials))
    }

    /// Whether there is a token to connect with. Blocks.
    pub(crate) fn signed_in(&self) -> Result<bool> {
        Ok(self.read()?.is_some_and(|c| c.token_response.is_some()))
    }

    /// Forgets the tokens. Blocks.
    pub(crate) fn clear_now(&self) -> Result<()> {
        self.vault.set(&self.key, None)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Tokens) -> Result<T> + Send + 'static,
    ) -> Result<T, AuthError> {
        let tokens = self.clone();
        tokio::task::spawn_blocking(move || f(&tokens))
            .await
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?
            .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))
    }
}

#[async_trait]
impl CredentialStore for Tokens {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.blocking(Tokens::read).await
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let text = serde_json::to_string(&Saved {
            url: self.url.clone(),
            credentials,
        })
        .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        self.blocking(move |t| t.vault.set(&t.key, Some(&text)))
            .await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.blocking(Tokens::clear_now).await
    }

    /// The authorization server won't refresh them any more: forget them, unless a sign-in has
    /// saved new ones since, so the server shows as needing a sign-in.
    async fn on_refresh_token_rejected(
        &self,
        rejected: &StoredCredentials,
    ) -> Result<(), AuthError> {
        let rejected = serde_json::to_value(rejected).ok();
        self.blocking(move |t| {
            if t.read()?.and_then(|c| serde_json::to_value(c).ok()) == rejected {
                t.clear_now()?;
            }
            Ok(())
        })
        .await
    }
}

/// The `WWW-Authenticate` challenge of a server that wants a sign-in, when `error` says that
/// is why it couldn't connect: `Some("")` when it says so without a challenge.
pub(crate) fn sign_in_challenge(error: &anyhow::Error) -> Option<String> {
    error.chain().find_map(|e| {
        // rmcp keeps the transport's error in a field the chain doesn't follow.
        if let Some(ClientInitializeError::TransportError { error, .. }) = e.downcast_ref() {
            return challenge_in(error.error.as_ref());
        }
        challenge_in(e)
    })
}

fn challenge_in(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut error = Some(error);
    while let Some(e) = error {
        if let Some(required) = e.downcast_ref::<AuthRequiredError>() {
            return Some(required.www_authenticate_header.clone());
        }
        if let Some(AuthError::AuthorizationRequired) = e.downcast_ref() {
            return Some(String::new());
        }
        error = e.source();
    }
    None
}

/// A sign-in started: the page to open in a browser, and the wait for it to come back.
pub(crate) struct SignIn {
    pub(crate) page: String,
    listener: TcpListener,
    state: OAuthState,
}

/// Finds the authorization server of the MCP server at `url`, registers xolkit with it, and
/// returns the sign-in page to open. `challenge` is the server's `WWW-Authenticate` header, when
/// it sent one. The tokens go to `tokens` once the browser comes back; see [`SignIn::finish`].
pub(crate) async fn start(url: &str, challenge: Option<&str>, tokens: Tokens) -> Result<SignIn> {
    // On loopback only: the browser comes back here, and nothing else can reach it.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("could not listen for the browser")?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let mut manager = AuthorizationManager::new(url)
        .await
        .context("could not set up the sign-in")?;
    manager.set_credential_store(tokens);
    let mut request = AuthorizationRequest::new(redirect)
        .with_client_name("xolkit")
        .with_initial_scope_selection();
    if let Some(challenge) = challenge.filter(|c| !c.is_empty()) {
        request = request.with_challenge(challenge);
    }
    let mut state = OAuthState::Unauthorized(manager);
    state
        .start_authorization(request)
        .await
        .map_err(|e| match e {
            AuthError::NoAuthorizationSupport => {
                anyhow::anyhow!("the server doesn't say how to sign in to it")
            }
            AuthError::RegistrationFailed(message) => anyhow::anyhow!(
                "the server wouldn't register xolkit ({message}); servers that need a client \
                 ID set up by hand aren't supported yet"
            ),
            e => anyhow::Error::new(e).context("could not start the sign-in"),
        })?;
    let page = state
        .get_authorization_url()
        .await
        .context("could not start the sign-in")?;
    Ok(SignIn {
        page,
        listener,
        state,
    })
}

impl SignIn {
    /// Waits for the browser to come back, and saves the tokens it brings.
    pub(crate) async fn finish(mut self) -> Result<()> {
        tokio::time::timeout(SIGN_IN_TIMEOUT, async {
            loop {
                let (mut stream, _) = self.listener.accept().await?;
                // Anything but the redirect (a favicon, a stray request) is turned away.
                let Ok(Ok(Some(target))) =
                    tokio::time::timeout(REQUEST_TIMEOUT, read_target(&mut stream)).await
                else {
                    let _ = respond(&mut stream, "404 Not Found", "Not found.").await;
                    continue;
                };
                let url = format!("http://127.0.0.1{target}");
                let result = match query(&url, "error") {
                    Some(error) => Err(anyhow::anyhow!(
                        "{}",
                        query(&url, "error_description").unwrap_or(error)
                    )),
                    None => self
                        .state
                        .handle_callback_url(&url)
                        .await
                        .context("could not finish the sign-in"),
                };
                let page = match &result {
                    Ok(()) => "Signed in. You can close this tab and go back to xolkit.".to_owned(),
                    Err(e) => format!("The sign-in failed: {e:#}"),
                };
                let status = if result.is_ok() {
                    "200 OK"
                } else {
                    "400 Bad Request"
                };
                let _ = respond(&mut stream, status, &page).await;
                return result;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("the browser didn't come back within ten minutes"))?
    }
}

/// The target of the `GET /callback` request on `stream`, or `None` for any other request.
async fn read_target(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut head = Vec::new();
    let mut buf = [0; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            bail!("the browser hung up");
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > MAX_REQUEST {
            bail!("the request is too long");
        }
    }
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next().unwrap_or_default().split(' ');
    Ok(match (parts.next(), parts.next()) {
        (Some("GET"), Some(target))
            if target == "/callback" || target.starts_with("/callback?") =>
        {
            Some(target.to_owned())
        }
        _ => None,
    })
}

fn query(url: &str, key: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>xolkit</title>\
         <body style=\"font:16px system-ui;margin:4em auto;max-width:36em\"><p>{}</p>",
        escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
