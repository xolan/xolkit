//! For tests: an MCP server on loopback that wants an OAuth sign-in, with its own authorization
//! server, which signs anyone in at once. Just enough HTTP for rmcp and a browser stand-in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Tokens in memory, for tests, not the OS keyring.
#[derive(Default)]
pub(crate) struct MemoryVault(pub(crate) Mutex<HashMap<String, String>>);

impl crate::oauth::Vault for MemoryVault {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: Option<&str>) -> Result<()> {
        let mut entries = self.0.lock().unwrap();
        match value {
            Some(value) => entries.insert(key.to_owned(), value.to_owned()),
            None => entries.remove(key),
        };
        Ok(())
    }
}

/// The access token it hands out, and the only one its MCP endpoint takes.
pub(crate) const TOKEN: &str = "fake-access-token";

pub(crate) struct FakeServer {
    /// Its MCP endpoint.
    pub(crate) url: String,
    /// How many clients registered with it.
    pub(crate) registrations: Arc<Mutex<usize>>,
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub(crate) async fn start() -> FakeServer {
    start_with(None).await
}

/// A fake whose metadata names `authorize` as its sign-in page, when given.
pub(crate) async fn start_with(authorize: Option<&str>) -> FakeServer {
    let authorize = authorize.map(str::to_owned);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let registrations = Arc::new(Mutex::new(0));
    let fake = FakeServer {
        url: format!("{base}/mcp"),
        registrations: registrations.clone(),
    };
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (base, registrations, authorize) =
                (base.clone(), registrations.clone(), authorize.clone());
            tokio::spawn(async move {
                let _ = serve(stream, &base, &registrations, authorize.as_deref()).await;
            });
        }
    });
    fake
}

async fn serve(
    mut stream: TcpStream,
    base: &str,
    registrations: &Mutex<usize>,
    authorize: Option<&str>,
) -> Option<()> {
    let request = read(&mut stream).await?;
    let (status, headers, body) = answer(&request, base, registrations, authorize);
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.ok()?;
    stream.write_all(body.as_bytes()).await.ok()?;
    stream.shutdown().await.ok()
}

fn answer(
    request: &HttpRequest,
    base: &str,
    registrations: &Mutex<usize>,
    authorize: Option<&str>,
) -> (&'static str, Vec<(&'static str, String)>, String) {
    let json_headers = vec![("Content-Type", "application/json".to_owned())];
    let ok = |body: Value| ("200 OK", json_headers.clone(), body.to_string());
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", path) if path.starts_with("/.well-known/oauth-protected-resource") => ok(json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
        })),
        ("GET", path) if path.starts_with("/.well-known/oauth-authorization-server") => ok(json!({
            "issuer": base,
            "authorization_endpoint": authorize
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{base}/authorize")),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
        })),
        ("POST", "/register") => {
            *registrations.lock().unwrap() += 1;
            let asked: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            (
                "201 Created",
                json_headers,
                json!({
                    "client_id": "fake-client",
                    "redirect_uris": asked["redirect_uris"],
                })
                .to_string(),
            )
        }
        // Signs in at once: straight back to the client with a code.
        ("GET", "/authorize") => {
            let mut back = url::Url::parse(&request.query["redirect_uri"]).unwrap();
            back.query_pairs_mut()
                .append_pair("code", "fake-code")
                .append_pair("state", &request.query["state"])
                .append_pair("iss", base);
            (
                "302 Found",
                vec![("Location", back.to_string())],
                String::new(),
            )
        }
        ("POST", "/token") => ok(json!({
            "access_token": TOKEN,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "fake-refresh-token",
        })),
        ("POST", "/mcp") => {
            let authorized = request
                .headers
                .get("authorization")
                .is_some_and(|v| v == &format!("Bearer {TOKEN}"));
            if !authorized {
                let challenge = format!(
                    "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
                );
                return (
                    "401 Unauthorized",
                    vec![("WWW-Authenticate", challenge)],
                    String::new(),
                );
            }
            let message: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let result = match message["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1"},
                }),
                Some("tools/list") => json!({
                    "tools": [{"name": "hello", "inputSchema": {"type": "object"}}],
                }),
                // Notifications get no answer.
                _ => return ("202 Accepted", Vec::new(), String::new()),
            };
            ok(json!({"jsonrpc": "2.0", "id": message["id"], "result": result}))
        }
        // No event stream, and no sessions to end.
        (_, "/mcp") => ("405 Method Not Allowed", Vec::new(), String::new()),
        _ => ("404 Not Found", Vec::new(), String::new()),
    }
}

async fn read(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut data = Vec::new();
    let mut buf = [0; 4096];
    let end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = data[end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let url = url::Url::parse(&format!("http://x{target}")).ok()?;
    Some(HttpRequest {
        method,
        path: url.path().to_owned(),
        query: url.query_pairs().into_owned().collect(),
        headers,
        body,
    })
}
