//! Signing in to remote MCP servers with OAuth. The daemon finds a server's authorization
//! server, registers xolkit with it, and catches the browser's redirect on a loopback port; the
//! app only opens the page. Tokens go to the OS keyring, bound to the server's URL, and rmcp
//! refreshes them as they run out.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
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

/// The most UTF-16 units of tokens kept in one keyring entry: Windows holds 2560 bytes in one,
/// and keeps text as UTF-16.
const CHUNK: usize = 1000;

/// Counts sign-outs per server, so a token write begun before one (a sign-in coming back, a
/// refresh) can't put the tokens back after it.
#[derive(Clone, Default)]
pub(crate) struct Epochs {
    /// Only ever held for a moment.
    counts: Arc<Mutex<HashMap<String, u64>>>,
    /// Held by every read and write of tokens, which may wait for the keyring to be unlocked.
    io: Arc<Mutex<()>>,
}

impl Epochs {
    fn current(&self, name: &str) -> u64 {
        self.counts.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

/// A server's tokens in the vault, bound to its URL: tokens saved for one address are never sent
/// to another, even when a server keeps its name and changes its URL. They are kept in pieces;
/// see [`write`].
#[derive(Clone)]
pub(crate) struct Tokens {
    vault: Arc<dyn Vault>,
    epochs: Epochs,
    name: String,
    url: String,
    /// The sign-outs there had been when these were made; after another, they can't write.
    epoch: u64,
}

impl Tokens {
    pub(crate) fn new(vault: Arc<dyn Vault>, epochs: Epochs, name: &str, url: &str) -> Tokens {
        let epoch = epochs.current(name);
        Tokens {
            vault,
            epochs,
            name: name.to_owned(),
            url: url.to_owned(),
            epoch,
        }
    }

    /// Forgets server `name`'s tokens, and stops any write under way from putting them back.
    /// Blocks.
    pub(crate) fn forget(vault: &dyn Vault, epochs: &Epochs, name: &str) -> Result<()> {
        let _io = epochs.io.lock().unwrap();
        *epochs
            .counts
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default() += 1;
        delete(vault, &oauth_key(name))
    }

    /// The saved credentials, if they are for this URL. Blocks.
    fn read(&self) -> Result<Option<StoredCredentials>> {
        let _io = self.epochs.io.lock().unwrap();
        self.read_locked()
    }

    fn read_locked(&self) -> Result<Option<StoredCredentials>> {
        let Some(text) = read(self.vault.as_ref(), &oauth_key(&self.name))? else {
            return Ok(None);
        };
        // Something unreadable is as good as nothing: signing in again replaces it.
        Ok(serde_json::from_str::<Saved>(&text)
            .ok()
            .filter(|saved| saved.url == self.url)
            .map(|saved| saved.credentials))
    }

    /// Runs `write` unless the server has been signed out of since these were made. Blocks.
    fn write(&self, write: impl FnOnce(&Tokens) -> Result<()>) -> Result<()> {
        let _io = self.epochs.io.lock().unwrap();
        anyhow::ensure!(
            self.epochs.current(&self.name) == self.epoch,
            "signed out of {} meanwhile",
            self.name
        );
        write(self)
    }

    /// Whether there is a token to connect with. Blocks.
    pub(crate) fn signed_in(&self) -> Result<bool> {
        Ok(self.read()?.is_some_and(|c| c.token_response.is_some()))
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

/// What the first piece under `key` says: the generation of the rest, and how many pieces there
/// are in all.
struct Head {
    generation: u64,
    count: usize,
    first: String,
}

fn head(vault: &dyn Vault, key: &str) -> Result<Option<Head>> {
    Ok(vault.get(key)?.and_then(|text| {
        let (line, first) = text.split_once('\n')?;
        let (generation, count) = line.split_once(' ')?;
        Some(Head {
            generation: generation.parse().ok()?,
            count: count.parse().ok()?,
            first: first.to_owned(),
        })
    }))
}

fn piece_key(key: &str, generation: u64, i: usize) -> String {
    format!("{key}/{generation}/{i}")
}

/// The text kept in pieces under `key`, if it is all there.
fn read(vault: &dyn Vault, key: &str) -> Result<Option<String>> {
    let Some(head) = head(vault, key)? else {
        return Ok(None);
    };
    let mut text = head.first;
    for i in 1..head.count {
        match vault.get(&piece_key(key, head.generation, i))? {
            Some(piece) => text.push_str(&piece),
            None => return Ok(None),
        }
    }
    Ok(Some(text))
}

/// Keeps `text` in pieces under `key`, replacing what was there: the first at `key`, saying how
/// many there are, the rest at `key/<generation>/<n>`. Generations take turns, 0 and 1: the new
/// one is written in full before the first piece points at it, so a write that fails halfway
/// leaves the old text whole. Pieces are written from 1 up and removed from the top down, so
/// whatever is left of a generation starts at 1, and [`sweep`] finds it without the first piece.
fn write(vault: &dyn Vault, key: &str, text: &str) -> Result<()> {
    let pieces = pieces(text);
    let old = head(vault, key)?.map_or(1, |h| h.generation);
    let generation = if old == 0 { 1 } else { 0 };
    // Anything left there by a write that failed.
    sweep(vault, key, generation, 1)?;
    let written = pieces
        .iter()
        .enumerate()
        .skip(1)
        .try_for_each(|(i, piece)| vault.set(&piece_key(key, generation, i), Some(piece)))
        .and_then(|()| {
            vault.set(
                key,
                Some(&format!("{generation} {}\n{}", pieces.len(), pieces[0])),
            )
        });
    if let Err(e) = written {
        let _ = sweep(vault, key, generation, 1);
        return Err(e);
    }
    // The old generation is no longer read; delete finds anything an error leaves of it.
    let _ = sweep(vault, key, 1 - generation, 1);
    Ok(())
}

/// Removes generation `generation`'s pieces from `from` up, top down.
fn sweep(vault: &dyn Vault, key: &str, generation: u64, from: usize) -> Result<()> {
    let mut end = from;
    while vault.get(&piece_key(key, generation, end))?.is_some() {
        end += 1;
    }
    for i in (from..end).rev() {
        vault.set(&piece_key(key, generation, i), None)?;
    }
    Ok(())
}

/// `text` in pieces of at most `CHUNK` UTF-16 units; at least one.
fn pieces(text: &str) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > CHUNK {
            pieces.push(String::new());
            units = 0;
        }
        units += c.len_utf16();
        pieces.last_mut().expect("never empty").push(c);
    }
    pieces
}

/// Removes every piece under `key`, of either generation, even ones a failed write left.
fn delete(vault: &dyn Vault, key: &str) -> Result<()> {
    sweep(vault, key, 0, 1)?;
    sweep(vault, key, 1, 1)?;
    vault.set(key, None)
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
        self.blocking(move |t| t.write(|t| write(t.vault.as_ref(), &oauth_key(&t.name), &text)))
            .await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.blocking(|t| t.write(|t| delete(t.vault.as_ref(), &oauth_key(&t.name))))
            .await
    }

    /// The authorization server won't refresh them any more: forget them, unless a sign-in has
    /// saved new ones since, so the server shows as needing a sign-in.
    async fn on_refresh_token_rejected(
        &self,
        rejected: &StoredCredentials,
    ) -> Result<(), AuthError> {
        let rejected = serde_json::to_value(rejected).ok();
        self.blocking(move |t| {
            t.write(|t| {
                if t.read_locked()?.and_then(|c| serde_json::to_value(c).ok()) == rejected {
                    delete(t.vault.as_ref(), &oauth_key(&t.name))?;
                }
                Ok(())
            })
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
    /// The `state` the page carries, which the browser brings back.
    csrf: String,
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
    // The app hands the page to the OS to open: a web page, nothing else a server may name.
    let parsed = url::Url::parse(&page).context("the server's sign-in page isn't a URL")?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "https" | "http"),
        "the server's sign-in page isn't a web page: {page}"
    );
    let csrf = query(&page, "state").context("the sign-in page carries no state")?;
    Ok(SignIn {
        page,
        listener,
        state,
        csrf,
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
                // Only the browser coming back from this sign-in's page ends it: anything else
                // on the machine could reach the port.
                if query(&url, "state").as_deref() != Some(self.csrf.as_str()) {
                    let _ = respond(
                        &mut stream,
                        "400 Bad Request",
                        "This isn't the sign-in xolkit is waiting for.",
                    )
                    .await;
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_oauth::MemoryVault;
    use rmcp::transport::auth::OAuthTokenResponse;

    fn credentials(token: &str) -> StoredCredentials {
        let response: OAuthTokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": token,
            "token_type": "Bearer",
            "refresh_token": "r",
        }))
        .unwrap();
        StoredCredentials::new("client".into(), Some(response), vec![], Some(1))
    }

    fn token(credentials: Option<StoredCredentials>) -> Option<String> {
        let value = serde_json::to_value(credentials?).unwrap();
        Some(value["token_response"]["access_token"].as_str()?.to_owned())
    }

    #[tokio::test]
    async fn long_tokens_are_kept_in_pieces_each_a_keyring_entry_holds() {
        let vault = Arc::new(MemoryVault::default());
        let tokens = Tokens::new(vault.clone(), Epochs::default(), "s", "https://x/mcp");
        // A JWT-sized token, past what one Windows credential holds.
        let long = format!("{}{}", "é".repeat(1500), "🔑".repeat(700));
        tokens.save(credentials(&long)).await.unwrap();
        let entries = vault.0.lock().unwrap().clone();
        assert!(entries.len() >= 3, "{:?}", entries.keys());
        assert!(
            entries
                .values()
                .all(|v| v.encode_utf16().count() * 2 <= 2560)
        );
        assert_eq!(token(tokens.load().await.unwrap()), Some(long));

        // A shorter one takes the place of all of it.
        tokens.save(credentials("short")).await.unwrap();
        assert_eq!(vault.0.lock().unwrap().len(), 1);
        assert_eq!(
            token(tokens.load().await.unwrap()).as_deref(),
            Some("short")
        );

        // Not for another URL.
        let moved = Tokens::new(vault.clone(), Epochs::default(), "s", "https://y/mcp");
        assert!(moved.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_save_under_way_cant_undo_a_sign_out() {
        let vault = Arc::new(MemoryVault::default());
        let epochs = Epochs::default();
        let tokens = Tokens::new(vault.clone(), epochs.clone(), "s", "https://x/mcp");
        tokens.save(credentials(&"t".repeat(2500))).await.unwrap();
        Tokens::forget(vault.as_ref(), &epochs, "s").unwrap();
        assert!(vault.0.lock().unwrap().is_empty());
        // A sign-in coming back, or a refresh, after the sign-out.
        assert!(tokens.save(credentials("late")).await.is_err());
        assert!(vault.0.lock().unwrap().is_empty());
        // A sign-in started after it saves.
        let fresh = Tokens::new(vault.clone(), epochs, "s", "https://x/mcp");
        fresh.save(credentials("new")).await.unwrap();
        assert_eq!(token(fresh.load().await.unwrap()).as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn opens_nothing_but_a_web_page() {
        let fake = crate::fake_oauth::start_with(Some("file:///etc/passwd")).await;
        let tokens = Tokens::new(
            Arc::new(MemoryVault::default()),
            Epochs::default(),
            "fake",
            &fake.url,
        );
        let refused = start(&fake.url, None, tokens).await.err().unwrap();
        assert!(
            format!("{refused:#}").contains("isn't a web page"),
            "{refused:#}"
        );
    }

    /// Fails every write once `fail_after` more have gone through.
    struct FlakyVault {
        inner: MemoryVault,
        fail_after: Mutex<Option<usize>>,
    }

    impl Vault for FlakyVault {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.inner.get(key)
        }

        fn set(&self, key: &str, value: Option<&str>) -> Result<()> {
            if let Some(left) = self.fail_after.lock().unwrap().as_mut() {
                anyhow::ensure!(*left > 0, "the keyring went away");
                *left -= 1;
            }
            self.inner.set(key, value)
        }
    }

    #[tokio::test]
    async fn a_write_that_fails_halfway_leaves_the_old_tokens_whole() {
        let vault = Arc::new(FlakyVault {
            inner: MemoryVault::default(),
            fail_after: Mutex::new(None),
        });
        let tokens = Tokens::new(vault.clone(), Epochs::default(), "s", "https://x/mcp");
        let old = "a".repeat(2500);
        tokens.save(credentials(&old)).await.unwrap();
        // Same length, so pieces of the two would fit together.
        *vault.fail_after.lock().unwrap() = Some(1);
        assert!(tokens.save(credentials(&"b".repeat(2500))).await.is_err());
        *vault.fail_after.lock().unwrap() = None;
        assert_eq!(token(tokens.load().await.unwrap()), Some(old));
    }

    #[tokio::test]
    async fn a_sign_out_leaves_nothing_a_failed_write_left_behind() {
        let vault = Arc::new(FlakyVault {
            inner: MemoryVault::default(),
            fail_after: Mutex::new(None),
        });
        let epochs = Epochs::default();
        let tokens = Tokens::new(vault.clone(), epochs.clone(), "s", "https://x/mcp");
        tokens.save(credentials(&"a".repeat(3500))).await.unwrap();
        // Every way a write can stop: after each of its keyring writes in turn.
        for fail_after in 0..8 {
            *vault.fail_after.lock().unwrap() = Some(fail_after);
            let _ = tokens.save(credentials(&"b".repeat(4500))).await;
        }
        *vault.fail_after.lock().unwrap() = None;
        Tokens::forget(vault.as_ref(), &epochs, "s").unwrap();
        let left = vault.inner.0.lock().unwrap().clone();
        assert!(left.is_empty(), "{:?}", left.keys());
    }
}
