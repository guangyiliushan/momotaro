//! Wire vocabulary for the Momotaro LAN API: the one place the desktop
//! server and (future) mobile controller agree on auth, pairing, and the
//! health payload. Types here are the *contract*; handlers live in
//! `momotaro-server`, clients bind to these shapes.
//!
//! Zero-UI, zero-IO by design: everything is pure functions and serde types,
//! so both sides can test the contract without a socket.

use serde::Serialize;

/// The query key (and, on the client, the storage key) for the pairing
/// token issued at `serve` startup.
pub const TOKEN_QUERY_KEY: &str = "token";

/// The HTTP header the controller sends on every authenticated call.
pub const AUTH_HEADER: &str = "Authorization";

/// The bearer scheme prefix in front of the token in [`AUTH_HEADER`].
pub const BEARER_PREFIX: &str = "Bearer ";

/// Builds the pairing URL the desktop side renders (as text now, as a QR
/// code later): `http://<host>:<port>/?token=<token>`. The token rides in
/// the query once, at pairing time; every call after that carries it in the
/// auth header.
///
/// `host` is whatever the caller decided is reachable from the phone (the
/// bind address is often `0.0.0.0`, which no phone can dial — resolving the
/// LAN IP is the caller's job).
pub fn build_pairing_url(host: &str, port: u16, token: &str) -> String {
    format!("http://{host}:{port}/?{TOKEN_QUERY_KEY}={token}")
}

/// Pulls the pairing token back out of a query string (`a=b&token=x&c=d`).
/// Tokens are hex, so no percent-decoding is needed on this path.
pub fn extract_token_from_query(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| *k == TOKEN_QUERY_KEY)
            .map(|(_, v)| v)
    })
}

/// `GET /api/health` — liveness plus the build the phone is talking to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthResponse {
    /// `"ok"` while the process answers at all; richer states belong to
    /// `doctor`, which is a different (heavier) endpoint.
    pub status: String,
    /// The server's own package version.
    pub version: String,
}

/// Every error the API returns, in one shape the controller can render
/// without guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_url_round_trips_through_the_extractor() {
        let url = build_pairing_url("192.168.1.7", 7731, "abc123");
        assert!(url.starts_with("http://192.168.1.7:7731/?"));
        let query = url.rsplit('?').next().expect("the url has a query");
        assert_eq!(extract_token_from_query(query), Some("abc123"));
    }

    #[test]
    fn the_extractor_ignores_other_keys_and_missing_tokens() {
        assert_eq!(extract_token_from_query("a=b"), None);
        assert_eq!(extract_token_from_query("tokeny=z"), None);
        assert_eq!(extract_token_from_query("token="), Some(""));
    }

    #[test]
    fn health_response_serializes_both_fields() {
        let body = serde_json::to_value(HealthResponse {
            status: "ok".to_owned(),
            version: "0.1.0".to_owned(),
        })
        .expect("health serializes");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], "0.1.0");
    }
}
