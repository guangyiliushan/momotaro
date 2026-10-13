//! LAN REST API over the Momotaro engine.
//!
//! Deliberately blocking and single-threaded: the engine behind it is a
//! blocking facade, the controller's commands are coarse and low-frequency,
//! and one request at a time is the cheapest correctness guarantee there is
//! (`ponytail:` a thread pool + request queue if a mobile client ever
//! outruns it). HTTP parsing is `tiny_http`, not hand-rolled sockets.
//!
//! Auth: every `/api/*` call must carry the token issued at startup —
//! `Authorization: Bearer <token>` or the pairing query parameter. An
//! unauthenticated caller always gets 401; nothing about the surface leaks.

use std::path::{Path, PathBuf};

use momotaro_protocol::{
    AUTH_HEADER, BEARER_PREFIX, ErrorResponse, HealthResponse, build_pairing_url,
    extract_token_from_query,
};
use momotaro_run::{format_error, search_workspace};
use serde::Deserialize;
use tiny_http::{Header, Method, Response, Server};

/// The default bind address: every interface, so a phone on the same LAN can
/// reach it; the pairing URL printed at startup names the address.
pub const DEFAULT_BIND: &str = "0.0.0.0:7731";

/// What the server needs to run: the workspace it serves, the socket it
/// binds, and the token every call must present.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The Momotaro workspace root (the directory holding `momotaro.toml`).
    pub workspace: PathBuf,
    /// `ip:port` to bind, e.g. `0.0.0.0:7731`.
    pub bind: String,
    /// The pairing token (hex; generate with [`generate_token`] or pass your
    /// own).
    pub token: String,
}

/// A fresh 32-byte hex token: the pairing credential for the LAN surface.
/// It never leaves the machine except through the pairing URL the operator
/// chooses to reveal.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS entropy source is available");
    hex::encode(bytes)
}

/// The URL the desktop side shows for pairing: the bind address as given
/// (`0.0.0.0` binds every interface but is not itself dialable — resolve
/// the LAN IP before rendering this as a QR code).
pub fn pairing_url(bind: &str, token: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "7731"));
    build_pairing_url(host, port.parse().unwrap_or(7731), token)
}

/// Blocking serve loop. Returns only on a socket-level failure; `Ctrl+C`
/// ends the process the usual way.
pub fn serve(config: ServerConfig) -> Result<std::convert::Infallible, ServerError> {
    let server = Server::http(config.bind.as_str())
        .map_err(|e| ServerError::Bind(config.bind.clone(), e.to_string()))?;
    serve_on(server, &config.workspace, &config.token)
}

/// [`serve`] split from its socket so tests can bind port 0 and learn the
/// real port from the listener.
pub fn serve_on(
    server: Server,
    workspace: &Path,
    token: &str,
) -> Result<std::convert::Infallible, ServerError> {
    eprintln!("momotaro server listening on {}", server.server_addr());
    eprintln!(
        "pairing url: {}",
        pairing_url(&server.server_addr().to_string(), token)
    );

    loop {
        let mut request = match server.recv() {
            Ok(request) => request,
            Err(error) => return Err(ServerError::Io(error)),
        };
        let url = request.url().to_owned();
        let method = request.method().to_owned();

        // Auth first, on the raw url+headers: the query may carry the token
        // (pairing call), the header carries it on everything after.
        if !is_authenticated(&url, &request, token) {
            respond_error(request, 401, "missing or invalid token");
            continue;
        }
        let (path, query) = match url.split_once('?') {
            Some((path, query)) => (path.to_owned(), query.to_owned()),
            None => (url.clone(), String::new()),
        };
        // A token that rode in the query still has to be the right one.
        if !extract_token_from_query(&query)
            .map(|presented| constant_time_eq(presented, token))
            .unwrap_or(true)
        {
            respond_error(request, 401, "invalid token");
            continue;
        }

        match (method, path.as_str()) {
            (Method::Get, "/api/health") => {
                let body = serde_json::to_string(&HealthResponse {
                    status: "ok".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                })
                .expect("health body serializes");
                respond_json(request, 200, &body);
            }
            (Method::Post, "/api/search") => {
                let mut body = String::new();
                let read = request.as_reader().read_to_string(&mut body);
                let outcome = match read {
                    Err(error) => Err(error.to_string()),
                    Ok(_) => serde_json::from_str::<SearchRequest>(&body)
                        .map_err(|e| e.to_string())
                        .and_then(|request| {
                            let root = workspace.to_str().expect("workspace path is utf-8");
                            search_workspace(root, &request.query, request.top)
                                .map(|output| {
                                    serde_json::to_string(&output).expect("hits serialize")
                                })
                                .map_err(|e| format_error(&e))
                        }),
                };
                match outcome {
                    Ok(json) => respond_json(request, 200, &json),
                    Err(message) => respond_error(request, 400, &message),
                }
            }
            _ => respond_error(request, 404, "not found"),
        }
    }
}

/// Server-side failure modes worth naming: a bind collision is operator
/// error (the port is taken), everything else is an I/O outage mid-loop.
#[derive(Debug)]
pub enum ServerError {
    /// The bind address could not be claimed.
    Bind(String, String),
    /// The accept loop failed mid-serve.
    Io(std::io::Error),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::Bind(addr, message) => write!(f, "cannot bind {addr}: {message}"),
            ServerError::Io(error) => write!(f, "server I/O error: {error}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// The search request body: the same vocabulary as `momotaro search`.
#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_top")]
    top: usize,
}

fn default_top() -> usize {
    10
}

fn respond_error(request: tiny_http::Request, status: u16, message: &str) {
    let body = serde_json::to_string(&ErrorResponse {
        error: message.to_owned(),
    })
    .expect("error body serializes");
    respond_json(request, status, &body);
}

fn is_authenticated(url: &str, request: &tiny_http::Request, token: &str) -> bool {
    let header_token = request.headers().iter().find_map(|header| {
        header
            .field
            .equiv(AUTH_HEADER)
            .then(|| header.value.as_str().strip_prefix(BEARER_PREFIX))
            .flatten()
            .map(str::to_owned)
    });
    let query_token = url
        .split_once('?')
        .and_then(|(_, query)| extract_token_from_query(query))
        .map(str::to_owned);
    match header_token.or(query_token) {
        Some(presented) => constant_time_eq(&presented, token),
        None => false,
    }
}

/// A comparison that does not leak its progress through timing. The token is
/// the only gate on the whole surface, so the check does not get to be the
/// cheap kind.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn json_content_type() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header")
}

fn respond_json(request: tiny_http::Request, status: u16, body: &str) {
    let response = Response::from_string(body.to_owned())
        .with_status_code(status)
        .with_header(json_content_type());
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constant_time_compare_agrees_with_the_naive_one() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn generated_tokens_are_hex_and_long() {
        let token = generate_token();
        assert_eq!(token.len(), 64, "32 bytes hex-encoded");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, generate_token());
    }

    #[test]
    fn pairing_url_splits_the_bind_address() {
        assert_eq!(
            pairing_url("0.0.0.0:7731", "tok"),
            build_pairing_url("0.0.0.0", 7731, "tok")
        );
    }

    /// The full auth gate over a real socket: no token and a wrong token are
    /// both 401, the right token (header form) passes with a JSON health
    /// body. Health needs no workspace behind it, so an empty tempdir is a
    /// valid backdrop.
    #[test]
    fn the_health_endpoint_gates_on_the_token() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let token = "feedfacebeef".to_owned();
        let server = Server::http("127.0.0.1:0").expect("bind port 0");
        let addr = server.server_addr().to_string();
        let workspace = dir.path().to_path_buf();
        let test_token = token.clone();
        let _handle = std::thread::spawn(move || {
            let _ = serve_on(server, &workspace, &test_token);
        });

        assert!(
            request_status(&addr, None).contains(" 401 "),
            "no token -> 401"
        );
        assert!(
            request_status(&addr, Some("wrongtoken")).contains(" 401 "),
            "wrong token -> 401"
        );
        let ok = request_status(&addr, Some(&token));
        assert!(ok.contains(" 200 "), "right token -> 200, got {ok}");
        assert!(ok.contains("application/json"));
        assert!(ok.contains("\"status\":\"ok\""));

        // Detached on purpose: the loop runs until the test process exits.
    }

    fn request_status(addr: &str, token: Option<&str>) -> String {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let http =
            format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n");
        use std::io::Write;
        stream.write_all(http.as_bytes()).expect("write request");
        let mut response = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut response);
        response
    }
}
