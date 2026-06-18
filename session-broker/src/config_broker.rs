//! HTTP client for `botwork-config-broker`.
//!
//! Mirrors `launcher.rs` in shape: thin wrapper around `hyper::client::conn`
//! that POSTs JSON to a TCP endpoint, returns a typed result, and pushes all
//! status-mapping decisions into the consumer.
//!
//! v0 wire contract (see `config-broker/README.md` and issue #75):
//!
//! Request:  `POST /resolve { "tenant", "namespace", "plugin" }`
//! Success:  `200` + descriptor JSON
//! Error:    4xx/5xx with `{ "error", "message" }` envelope
//!
//! Trust posture: credless. The client does not authenticate; the network
//! membership of the docker `botwork` network is the trust boundary.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::log_info;

/// Env var name under which compact-JSON structured config is injected into
/// the plugin container. Reserved at every gate.
///
/// Kept here (not in the config-broker crate) because session-broker is the
/// thing that *injects* the env entry. The wire response carries the value
/// already-serialised; this constant is the receiving-end of that contract.
pub const CONFIG_ENV_NAME: &str = "BOTWORK_MCP_CONFIG";

/// Plugin descriptor as returned over the wire by config-broker's `/resolve`.
///
/// All fields the broker is contractually required to populate are
/// non-`Option`. Only `config_blob` is optional, signalling absence (operator
/// did not set `config:`) rather than empty (`""` / `{}`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PluginDescriptor {
    pub image: String,
    pub port: u16,
    pub path: String,
    pub upstream_auth: UpstreamAuth,
    #[serde(default)]
    pub resources: PluginResources,
    #[serde(default)]
    pub env: Vec<EnvEntry>,
    /// Already a compact-JSON string when present. Drop verbatim into
    /// `BOTWORK_MCP_CONFIG`; do not re-parse and do not re-serialise.
    #[serde(default)]
    pub config_blob: Option<String>,
}

/// Upstream-auth policy parsed from the wire string `"none"` or
/// `"bearer/<service>"`. Custom deserializer: tolerates the wire's flat
/// string shape rather than a tagged enum.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpstreamAuth {
    #[default]
    None,
    Bearer {
        service: String,
    },
}

impl<'de> Deserialize<'de> for UpstreamAuth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if raw == "none" {
            return Ok(Self::None);
        }
        if let Some(service) = raw.strip_prefix("bearer/") {
            if !service.is_empty()
                && !service.contains('/')
                && !service.chars().any(char::is_whitespace)
            {
                return Ok(Self::Bearer {
                    service: service.to_string(),
                });
            }
        }
        Err(serde::de::Error::custom(format!(
            "invalid upstream_auth: {raw:?}"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PluginResources {
    #[serde(default)]
    pub cpus: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub pids: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EnvEntry {
    pub name: String,
    pub value: String,
}

/// All ways a `/resolve` call can fail. Each variant carries enough detail to
/// build the immediate-response that ext_proc returns to the client.
#[derive(Debug, thiserror::Error)]
pub enum ConfigBrokerError {
    /// Body was missing / unparseable / required field absent / regex mismatch.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Namespace did not match the regex (separate code so future per-tenant
    /// namespace policy can be distinguished from "syntax").
    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),
    /// Plugin name not in the registry.
    #[error("unknown plugin: {0}")]
    UnknownPlugin(String),
    /// Server-side problem (bad config-broker state, parse failure on its
    /// side, etc.). Client retried-by-spawn won't help.
    #[error("config-broker internal error: {0}")]
    Internal(String),
    /// Upstream signalled transient unavailability.
    #[error("config-broker unavailable: {0}")]
    Unavailable(String),
    /// Couldn't reach config-broker at all (DNS, TCP, TLS, timeout).
    #[error("transport error contacting config-broker: {0}")]
    Transport(String),
    /// Got a response but couldn't parse it as a descriptor (or as the
    /// shared error envelope).
    #[error("bad response from config-broker: {0}")]
    BadResponse(String),
}

impl ConfigBrokerError {
    /// HTTP status code session-broker should surface to the *client* when
    /// this error fires during spawn. 4xx are operator-visible faults; 5xx
    /// (including transport) are infrastructure faults that look like a
    /// "spawn failed" — same shape as launcher unavailability today.
    pub fn status_code(&self) -> u32 {
        match self {
            // Pass-through 4xx: the client/operator caused these.
            Self::InvalidRequest(_) => 400,
            Self::InvalidNamespace(_) => 400,
            Self::UnknownPlugin(_) => 404,
            // Everything else surfaces as 502: config-broker is upstream of
            // session-broker and we couldn't get a clean answer out of it.
            Self::Internal(_)
            | Self::Unavailable(_)
            | Self::Transport(_)
            | Self::BadResponse(_) => 502,
        }
    }
}

#[derive(Debug, Serialize)]
struct ResolveRequest<'a> {
    tenant: &'a str,
    namespace: &'a str,
    plugin: &'a str,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: Option<String>,
    message: Option<String>,
}

/// Resolve a plugin descriptor from config-broker.
///
/// `endpoint` is the base URL (no trailing slash required); `/resolve` is
/// appended internally. `request_timeout` covers connect + send + receive.
pub async fn resolve(
    endpoint: &str,
    tenant: &str,
    namespace: &str,
    plugin: &str,
    request_timeout: Duration,
) -> Result<PluginDescriptor, ConfigBrokerError> {
    let url = format!("{}/resolve", endpoint.trim_end_matches('/'));
    log_info(&format!(
        "config-broker resolve: tenant={tenant} namespace={namespace} plugin={plugin} url={url}"
    ));

    let body = serde_json::to_vec(&ResolveRequest {
        tenant,
        namespace,
        plugin,
    })
    .map_err(|e| ConfigBrokerError::Transport(format!("encode request: {e}")))?;

    let result = async {
        let uri: Uri = url
            .parse()
            .map_err(|e| ConfigBrokerError::Transport(format!("invalid endpoint URL: {e}")))?;
        let host = uri
            .host()
            .ok_or_else(|| {
                ConfigBrokerError::Transport("missing host in config-broker endpoint".to_string())
            })?
            .to_string();
        let port = uri.port_u16().unwrap_or(80);
        let authority = if let Some(explicit_port) = uri.port_u16() {
            format!("{host}:{explicit_port}")
        } else {
            host.clone()
        };
        let path = uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/resolve".to_string());

        let mut sender = timeout(request_timeout, async {
            let stream = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?;
            let io = TokioIo::new(stream);
            let (sender, conn) = http1::handshake(io)
                .await
                .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(err) = conn.await {
                    log_info(&format!("config-broker HTTP connection error: {err}"));
                }
            });
            Ok::<_, ConfigBrokerError>(sender)
        })
        .await
        .map_err(|e| ConfigBrokerError::Transport(e.to_string()))??;

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{authority}{path}"))
            .header("Host", authority)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len().to_string())
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?;

        let response = timeout(request_timeout, sender.send_request(request))
            .await
            .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?
            .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let response_body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| ConfigBrokerError::Transport(e.to_string()))?
            .to_bytes()
            .to_vec();

        match status {
            200 => serde_json::from_slice::<PluginDescriptor>(&response_body)
                .map_err(|e| ConfigBrokerError::BadResponse(format!("decode descriptor: {e}"))),
            other => Err(error_from_envelope(other, &response_body)),
        }
    }
    .await;

    match &result {
        Ok(descriptor) => log_info(&format!(
            "config-broker resolve: ok image={} port={} upstream_auth={} config_blob={}",
            descriptor.image,
            descriptor.port,
            match &descriptor.upstream_auth {
                UpstreamAuth::None => "none".to_string(),
                UpstreamAuth::Bearer { service } => format!("bearer/{service}"),
            },
            descriptor
                .config_blob
                .as_ref()
                .map(|s| format!("present({} bytes)", s.len()))
                .unwrap_or_else(|| "absent".to_string()),
        )),
        Err(err) => log_info(&format!("config-broker resolve: error {err}")),
    }

    result
}

fn error_from_envelope(status: u16, body: &[u8]) -> ConfigBrokerError {
    let detail = serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|env| {
            let code = env.error.unwrap_or_default();
            let message = env.message.unwrap_or_default();
            if code.is_empty() && message.is_empty() {
                None
            } else {
                Some((code, message))
            }
        });

    match (status, detail) {
        (400, Some((code, message))) if code == "invalid_namespace" => {
            ConfigBrokerError::InvalidNamespace(message)
        }
        (400, Some((_, message))) => ConfigBrokerError::InvalidRequest(message),
        (404, Some((_, message))) => ConfigBrokerError::UnknownPlugin(message),
        (500, Some((_, message))) => ConfigBrokerError::Internal(message),
        (503, Some((_, message))) => ConfigBrokerError::Unavailable(message),
        // Any other status (or missing/unparseable envelope) is a server we
        // don't understand — treat as bad response, status drives the public
        // 502.
        (status, detail) => ConfigBrokerError::BadResponse(format!(
            "HTTP {status}: {}",
            detail
                .map(|(code, msg)| format!("{code}: {msg}"))
                .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_auth_none_round_trips() {
        let value: UpstreamAuth = serde_json::from_str("\"none\"").expect("decode");
        assert_eq!(value, UpstreamAuth::None);
    }

    #[test]
    fn upstream_auth_bearer_round_trips() {
        let value: UpstreamAuth = serde_json::from_str("\"bearer/github.com\"").expect("decode");
        assert_eq!(
            value,
            UpstreamAuth::Bearer {
                service: "github.com".to_string()
            }
        );
    }

    #[test]
    fn upstream_auth_rejects_garbage() {
        for bad in [
            "\"\"",
            "\"bearer\"",
            "\"bearer/\"",
            "\"bearer/foo bar\"",
            "\"bearer/foo/bar\"",
            "\"vault\"",
        ] {
            assert!(
                serde_json::from_str::<UpstreamAuth>(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn descriptor_decodes_minimal_shape() {
        let body = r#"{
            "image": "botwork/x:local",
            "port": 8000,
            "path": "/",
            "upstream_auth": "none"
        }"#;
        let descriptor: PluginDescriptor = serde_json::from_str(body).expect("decode");
        assert_eq!(descriptor.image, "botwork/x:local");
        assert!(descriptor.env.is_empty());
        assert_eq!(descriptor.resources, PluginResources::default());
        assert!(descriptor.config_blob.is_none());
    }

    #[test]
    fn descriptor_decodes_full_shape() {
        let body = r#"{
            "image": "botwork/github:local",
            "port": 8001,
            "path": "/mcp",
            "upstream_auth": "bearer/github.com",
            "resources": { "memory": "4g", "pids": 1024 },
            "env": [ { "name": "GITHUB_TOOLSETS", "value": "default,actions" } ],
            "config_blob": "{\"routes\":[]}"
        }"#;
        let descriptor: PluginDescriptor = serde_json::from_str(body).expect("decode");
        assert_eq!(descriptor.path, "/mcp");
        assert_eq!(
            descriptor.upstream_auth,
            UpstreamAuth::Bearer {
                service: "github.com".to_string()
            }
        );
        assert_eq!(descriptor.resources.memory.as_deref(), Some("4g"));
        assert_eq!(descriptor.resources.pids, Some(1024));
        assert!(descriptor.resources.cpus.is_none());
        assert_eq!(descriptor.env.len(), 1);
        assert_eq!(descriptor.config_blob.as_deref(), Some(r#"{"routes":[]}"#));
    }

    #[test]
    fn error_envelope_maps_400_invalid_namespace() {
        let body = br#"{"error":"invalid_namespace","message":"bad"}"#;
        let err = error_from_envelope(400, body);
        assert!(matches!(err, ConfigBrokerError::InvalidNamespace(_)));
        assert_eq!(err.status_code(), 400);
    }

    #[test]
    fn error_envelope_maps_400_invalid_request() {
        let body = br#"{"error":"invalid_request","message":"missing 'plugin'"}"#;
        let err = error_from_envelope(400, body);
        assert!(matches!(err, ConfigBrokerError::InvalidRequest(_)));
        assert_eq!(err.status_code(), 400);
    }

    #[test]
    fn error_envelope_maps_404_unknown_plugin() {
        let body = br#"{"error":"unknown_plugin","message":"unknown plugin: foo"}"#;
        let err = error_from_envelope(404, body);
        assert!(matches!(err, ConfigBrokerError::UnknownPlugin(_)));
        assert_eq!(err.status_code(), 404);
    }

    #[test]
    fn error_envelope_maps_500_internal() {
        let body = br#"{"error":"internal","message":"db down"}"#;
        let err = error_from_envelope(500, body);
        assert!(matches!(err, ConfigBrokerError::Internal(_)));
        assert_eq!(err.status_code(), 502);
    }

    #[test]
    fn error_envelope_unparseable_falls_through_to_bad_response() {
        let body = b"<html>boom</html>";
        let err = error_from_envelope(418, body);
        assert!(matches!(err, ConfigBrokerError::BadResponse(_)));
        assert_eq!(err.status_code(), 502);
    }
}
