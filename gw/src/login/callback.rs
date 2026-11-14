//! Loopback OAuth callback listener plus the paste-a-redirect-URL fallback.
//!
//! Ported from opencodex `oauth/callback-server.ts`: a one-shot HTTP listener on the
//! provider's registered port and path, racing a stdin paste prompt so a browser on
//! another machine can still finish the flow.

use anyhow::{anyhow, Result};
use std::io::IsTerminal;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::ui;

/// How long to wait for a browser redirect or a paste before giving up.
const TIMEOUT: Duration = Duration::from_secs(300);

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset='utf-8'><title>gw</title></head>\
<body style='font-family:system-ui,sans-serif;text-align:center;padding:4rem;color:#111'>\
<h2>&#9989; Login complete</h2><p>You can close this tab and return to the terminal.</p></body></html>";

const PASTE_PROMPT: &str = "Paste redirect URL or code (or wait for browser):";

/// An authorization response: the code plus the state it arrived with.
#[derive(Clone, Debug, PartialEq)]
pub struct Callback {
    pub code: String,
    pub state: Option<String>,
}

/// Syntactic shape of a pasted value, which decides whether `state` must be checked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// An absolute URL carrying the authorization response.
    Url,
    /// A bare query string such as `code=...&state=...`.
    Query,
    /// A raw code, optionally `code#state`.
    Raw,
}

/// Result of reading a pasted value.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    pub kind: Kind,
    pub code: Option<String>,
    pub state: Option<String>,
}

/// Percent-decode plus `+` as space, matching `URLSearchParams`.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of the first `key` in a `a=b&c=d` string.
fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (decode(name) == key).then(|| decode(value))
    })
}

fn parse_params(query: &str) -> (Option<String>, Option<String>) {
    (param(query, "code"), param(query, "state"))
}

/// Parse a redirect URL, a query string, or a raw `code[#state]` paste.
pub fn parse_callback_input(input: &str) -> Parsed {
    let value = input.trim();
    if value.is_empty() {
        return Parsed {
            kind: Kind::Raw,
            code: None,
            state: None,
        };
    }

    if let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    {
        // Parameters live in the query, or in the fragment for the rare provider
        // that answers there. They are never mixed across the two.
        let (before_hash, fragment) = match rest.split_once('#') {
            Some((before, frag)) => (before, frag),
            None => (rest, ""),
        };
        let query = before_hash.split_once('?').map(|(_, q)| q).unwrap_or("");
        let (code, state) = if param(query, "code").is_some() {
            parse_params(query)
        } else {
            parse_params(fragment)
        };
        return Parsed {
            kind: Kind::Url,
            code,
            state,
        };
    }

    if value.contains("code=") {
        let (code, state) = parse_params(value.trim_start_matches(['?', '#']));
        return Parsed {
            kind: Kind::Query,
            code,
            state,
        };
    }

    let (code, state) = match value.split_once('#') {
        Some((code, state)) => (code, Some(state.to_string())),
        None => (value, None),
    };
    Parsed {
        kind: Kind::Raw,
        code: Some(code.to_string()).filter(|c| !c.is_empty()),
        state: state.filter(|s| !s.is_empty()),
    }
}

/// Accept a pasted value only when it carries the expected state.
///
/// A URL/query-shaped paste is an authorization response and must match; a raw code
/// with an explicit `#state` suffix must match too. A bare in-session code is exempt.
pub fn accept_paste(input: &str, expected_state: &str) -> Result<Callback> {
    let parsed = parse_callback_input(input);
    let code = parsed
        .code
        .ok_or_else(|| anyhow!("no authorization code in that input"))?;
    let state_bearing = parsed.kind != Kind::Raw || parsed.state.is_some();
    if state_bearing && parsed.state.as_deref() != Some(expected_state) {
        return Err(anyhow!("state mismatch - possible CSRF attack"));
    }
    Ok(Callback {
        code,
        state: parsed.state.or_else(|| Some(expected_state.to_string())),
    })
}

/// Extract `(code, state, error)` from a callback request line.
fn inspect_request(line: &str, path: &str) -> std::result::Result<Callback, String> {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        return Err("method not allowed".into());
    }
    let (route, query) = target.split_once('?').unwrap_or((target, ""));
    if route != path {
        return Err("not found".into());
    }
    if let Some(error) = param(query, "error") {
        let description = param(query, "error_description").unwrap_or_else(|| error.clone());
        return Err(format!("authorization failed: {description}"));
    }
    let code = param(query, "code").ok_or("missing authorization code")?;
    Ok(Callback {
        code,
        state: param(query, "state"),
    })
}

fn respond(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Open `url` in the desktop browser; failure is not an error, the URL was printed.
pub fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Bind the callback listener, falling back to an ephemeral port when `fallback` is set.
pub async fn bind(port: u16, fallback: bool) -> Result<(TcpListener, u16)> {
    match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => {
            let actual = listener.local_addr()?.port();
            Ok((listener, actual))
        }
        Err(e) if fallback && port != 0 => {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
            let actual = listener.local_addr()?.port();
            ui::warn(&format!("port {port} unavailable ({e}), using {actual}"));
            Ok((listener, actual))
        }
        Err(e) => Err(anyhow!("cannot listen on 127.0.0.1:{port}: {e}")),
    }
}

/// Wait for the redirect on `listener` or a matching paste on stdin.
pub async fn wait(
    listener: TcpListener,
    path: &str,
    expected_state: &str,
    allow_paste: bool,
) -> Result<Callback> {
    let listener_task = serve(listener, path, expected_state);
    let paste = allow_paste && std::io::stdin().is_terminal();
    let paste_task = async {
        if !paste {
            return std::future::pending::<Result<Callback>>().await;
        }
        loop {
            let input = prompt().await?;
            match accept_paste(&input, expected_state) {
                Ok(cb) => return Ok(cb),
                Err(err) => ui::warn(&err.to_string()),
            }
        }
    };
    tokio::select! {
        result = tokio::time::timeout(TIMEOUT, listener_task) => match result {
            Ok(cb) => cb,
            Err(_) => Err(anyhow!("timed out waiting for the browser callback")),
        },
        result = paste_task => result,
    }
}

async fn prompt() -> Result<String> {
    ui::info(PASTE_PROMPT);
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_string())
    })
    .await?
}

/// Serve one callback request, then stop. Invalid state is rejected, not accepted.
async fn serve(listener: TcpListener, path: &str, expected_state: &str) -> Result<Callback> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..read]);
        let line = request.lines().next().unwrap_or_default().to_string();
        let reply = match inspect_request(&line, path) {
            Ok(callback) if callback.state.as_deref() == Some(expected_state) => {
                let _ = socket.write_all(respond("200 OK", SUCCESS_HTML).as_bytes()).await;
                let _ = socket.shutdown().await;
                return Ok(callback);
            }
            Ok(_) => (
                "400 Bad Request",
                "state mismatch - possible CSRF attack".to_string(),
            ),
            Err(message) => ("400 Bad Request", message),
        };
        let body = format!(
            "<!doctype html><html><head><meta charset='utf-8'><title>gw</title></head>\
<body style='font-family:system-ui,sans-serif;text-align:center;padding:4rem;color:#111'>\
<h2>&#9888; Login failed</h2><p>{}</p></body></html>",
            reply.1.replace('<', "&lt;")
        );
        let _ = socket.write_all(respond(reply.0, &body).as_bytes()).await;
        let _ = socket.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "abc123";

    #[test]
    fn parses_a_full_redirect_url() {
        let parsed = parse_callback_input(
            "http://localhost:54545/callback?code=xyz&state=abc123#ignored",
        );
        assert_eq!(parsed.kind, Kind::Url);
        assert_eq!(parsed.code.as_deref(), Some("xyz"));
        assert_eq!(parsed.state.as_deref(), Some(STATE));
    }

    #[test]
    fn parses_a_query_string_and_a_raw_code() {
        let query = parse_callback_input("?code=xyz&state=abc123");
        assert_eq!(query.kind, Kind::Query);
        assert_eq!(query.code.as_deref(), Some("xyz"));

        let raw = parse_callback_input("  xyz#abc123 ");
        assert_eq!(raw.kind, Kind::Raw);
        assert_eq!(raw.code.as_deref(), Some("xyz"));
        assert_eq!(raw.state.as_deref(), Some(STATE));

        let bare = parse_callback_input("xyz");
        assert_eq!(bare.code.as_deref(), Some("xyz"));
        assert_eq!(bare.state, None);
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let parsed = parse_callback_input(
            "http://127.0.0.1:1/callback?code=a%2Fb%2Bc&state=abc123",
        );
        assert_eq!(parsed.code.as_deref(), Some("a/b+c"));
        let plus = parse_callback_input("?code=a+b&state=abc123");
        assert_eq!(plus.code.as_deref(), Some("a b"));
    }

    #[test]
    fn state_mismatch_is_rejected() {
        assert!(accept_paste("http://localhost/callback?code=x&state=wrong", STATE).is_err());
        assert!(accept_paste("?code=x&state=wrong", STATE).is_err());
        assert!(accept_paste("x#wrong", STATE).is_err());
        assert!(accept_paste("http://localhost/callback?code=x", STATE).is_err());
    }

    #[test]
    fn matching_or_bare_pastes_are_accepted() {
        let url = accept_paste("http://localhost/callback?code=x&state=abc123", STATE).unwrap();
        assert_eq!(url.code, "x");
        let bare = accept_paste("x", STATE).unwrap();
        assert_eq!(bare.state.as_deref(), Some(STATE));
    }

    #[test]
    fn request_line_is_parsed_and_errors_reported() {
        let ok = inspect_request("GET /callback?code=x&state=abc123 HTTP/1.1", "/callback").unwrap();
        assert_eq!(ok.code, "x");
        assert!(inspect_request("GET /callback?code=x HTTP/1.1", "/callback").unwrap().state.is_none());
        assert!(inspect_request("GET /other?code=x HTTP/1.1", "/callback").is_err());
        assert!(inspect_request(
            "GET /callback?error=access_denied&error_description=nope HTTP/1.1",
            "/callback"
        )
        .unwrap_err()
        .contains("nope"));
    }

    #[tokio::test]
    async fn serves_one_redirect_and_reports_state() {
        let (listener, port) = bind(0, false).await.unwrap();
        let task = tokio::spawn(serve(listener, "/callback", STATE));
        let body = format!("GET /callback?code=xyz&state={STATE} HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let callback = task.await.unwrap().unwrap();
        assert_eq!(callback.code, "xyz");
        assert!(response.contains("200 OK"));
    }
}
