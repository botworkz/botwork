use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value};

use crate::cmd::log_info;
use crate::config::Config;
use crate::docker::{self, ContainerLaunch};
use crate::error::LauncherError;
use crate::mount;
use crate::validate::{is_sensitive_env, Validators};

const MAX_JSON_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub validators: Validators,
}

struct LaunchRequest<'a> {
    name: &'a str,
    image: &'a str,
    /// Resolved at parse time: the payload may omit `network` and inherit the
    /// launcher's `BOTWORK_LAUNCHER_DEFAULT_NETWORK`. Owned so the resolved
    /// value can outlive the original payload borrow.
    network: String,
    staging_path: &'a str,
    with_workspace: bool,
    pids_limit: Option<u32>,
    cpu_limit: Option<&'a str>,
    memory_limit: Option<&'a str>,
    env: Vec<(String, String)>,
    /// RFE #105 round-3: caller-supplied docker labels, threaded
    /// through to `docker run --label key=value` in declaration order.
    /// session-broker uses this to stamp every spawned container with
    /// its `(tenant, workspace, plugin)` identity so the cold-start
    /// recovery path can `docker ps --filter name=mcp_session_*` +
    /// `docker inspect` and rebuild routing state by joining the
    /// labels against the `session_worker` table (round-3 PR2).
    ///
    /// Owned `Vec<(String, String)>` rather than borrowed slice on
    /// the parsed view because the validator copies values out of the
    /// JSON payload; same shape as `env`.
    labels: Vec<(String, String)>,
}

#[cfg_attr(test, derive(Debug))]
struct BindAgentRequest<'a> {
    staging_path: &'a str,
    agent_dir: &'a str,
}

#[cfg_attr(test, derive(Debug))]
struct TeardownRequest<'a> {
    name: &'a str,
    staging_path: &'a str,
}

pub async fn handle_request(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match dispatch_request(request, &state).await {
        Ok(response) => response,
        Err(error) => error_response(error),
    };
    Ok(response)
}

async fn dispatch_request(
    request: Request<Incoming>,
    state: &AppState,
) -> Result<Response<Full<Bytes>>, LauncherError> {
    log_info(&format!(
        "request: method={} path={}",
        request.method(),
        request.uri().path()
    ));

    if request.method() != Method::POST {
        return Err(LauncherError::NotFound("not found".to_string()));
    }

    match request.uri().path() {
        "/launch" => handle_launch(request, state).await,
        "/bind-agent" => handle_bind_agent(request, state).await,
        "/teardown" => handle_teardown(request, state).await,
        _ => Err(LauncherError::NotFound("not found".to_string())),
    }
}

async fn handle_launch(
    request: Request<Incoming>,
    state: &AppState,
) -> Result<Response<Full<Bytes>>, LauncherError> {
    let payload = parse_json_object(request).await?;
    let launch = parse_launch_payload(&payload, &state.validators, &state.config.default_network)?;

    // When the launcher is configured with an egress proxy, splice the
    // canonical proxy env vars into the per-spawn env LIST and on into
    // the container. Done here at the spawn site (rather than in the
    // payload parser) so:
    //
    //   1. Payload validation rules for caller-supplied env vars stay
    //      untouched (the launcher-injected entries are not subject to
    //      `valid_env_name` / `is_sensitive_env` checks because they
    //      are operator-trusted and never touch argv via the secret
    //      path).
    //   2. The decision "should this container get the proxy at all?"
    //      lives next to the call into docker, so a future "skip proxy
    //      for this specific image" knob has one place to add the
    //      condition.
    //   3. The session-broker payload doesn't have to know about it —
    //      session-broker just keeps forwarding per-plugin env from
    //      config-broker, and the proxy injection is purely a
    //      deploy-time wiring concern owned by whoever runs the
    //      launcher unit.
    //
    // Caller-supplied env wins if it sets the same name (we never
    // shadow a deliberate override). Realistically nothing should be
    // setting these in the registry today; the precedence rule is
    // defensive and makes the behaviour obvious to a future debugger.
    let env = inject_proxy_env(launch.env, state.config.egress_proxy.as_deref());

    let outcome = docker::ensure_container(
        &ContainerLaunch {
            name: launch.name,
            image: launch.image,
            network: &launch.network,
            staging_path: launch.staging_path,
            with_workspace: launch.with_workspace,
            plugin_uid: state.config.plugin_uid,
            plugin_gid: state.config.plugin_gid,
            pids_limit: launch
                .pids_limit
                .unwrap_or(state.config.container_pids_limit),
            cpu_limit: launch
                .cpu_limit
                .unwrap_or(&state.config.container_cpu_limit),
            memory_limit: launch
                .memory_limit
                .unwrap_or(&state.config.container_memory_limit),
            read_only_rootfs: state.config.container_read_only_rootfs,
            env: &env,
            // RFE #105 round-3: caller-supplied docker labels. The
            // launcher passes these through verbatim — the validator
            // already enforces shape/length caps + reserved-prefix
            // rules at the wire boundary, so docker.rs treats the
            // slice as trusted.
            labels: &launch.labels,
        },
        &state.validators,
    )?;

    log_info(&format!(
        "launch ok: name={} image={} network={} staging_path={} env_count={} label_count={} ip={}",
        launch.name,
        launch.image,
        launch.network,
        launch.staging_path,
        env.len(),
        launch.labels.len(),
        outcome.container_ip,
    ));

    // `container_ip` is new in 0.1.5; older session-broker builds tolerate
    // unknown fields (serde default-on-missing) so this is wire-compatible.
    Ok(json_response(
        StatusCode::OK,
        &[
            "name",
            launch.name,
            "status",
            outcome.status,
            "container_ip",
            outcome.container_ip.as_str(),
        ],
    ))
}

async fn handle_bind_agent(
    request: Request<Incoming>,
    state: &AppState,
) -> Result<Response<Full<Bytes>>, LauncherError> {
    let payload = parse_json_object(request).await?;
    let bind = parse_bind_agent_payload(&payload, &state.validators)?;

    mount::bind_agent(
        bind.staging_path,
        bind.agent_dir,
        &state.validators,
        state.config.plugin_uid,
        state.config.plugin_gid,
    )?;

    log_info(&format!(
        "bind-agent ok: staging_path={} agent_dir={}",
        bind.staging_path, bind.agent_dir
    ));

    Ok(json_response(StatusCode::OK, &["status", "bound"]))
}

async fn handle_teardown(
    request: Request<Incoming>,
    state: &AppState,
) -> Result<Response<Full<Bytes>>, LauncherError> {
    let payload = parse_json_object(request).await?;
    let teardown = parse_teardown_payload(&payload, &state.validators)?;

    docker::teardown(teardown.name, teardown.staging_path, &state.validators)?;

    log_info(&format!(
        "teardown ok: name={} staging_path={}",
        teardown.name, teardown.staging_path
    ));

    Ok(json_response(StatusCode::OK, &["status", "torn_down"]))
}

async fn parse_json_object<B>(request: Request<B>) -> Result<Map<String, Value>, LauncherError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let body = read_request_body(request.into_body()).await?;

    match parse_json_bytes(&body)? {
        Value::Object(obj) => Ok(obj),
        _ => Err(LauncherError::BadRequest(
            "request body must be a JSON object".to_string(),
        )),
    }
}

async fn read_request_body<B>(body: B) -> Result<Bytes, LauncherError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut body = body;
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| {
            LauncherError::Internal(format!("failed to read request body: {err}"))
        })?;
        if let Ok(chunk) = frame.into_data() {
            if bytes.len().saturating_add(chunk.len()) > MAX_JSON_BODY_BYTES {
                // This socket is the only thing between a local uid and root — do not allow body DoS.
                return Err(LauncherError::PayloadTooLarge(format!(
                    "request body exceeds {MAX_JSON_BODY_BYTES} bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(Bytes::from(bytes))
}

fn parse_json_bytes(body: &[u8]) -> Result<Value, LauncherError> {
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    serde_json::from_slice::<Value>(body)
        .map_err(|_| LauncherError::BadRequest("request body must be valid JSON".to_string()))
}

fn parse_launch_payload<'a>(
    payload: &'a Map<String, Value>,
    validators: &Validators,
    default_network: &str,
) -> Result<LaunchRequest<'a>, LauncherError> {
    const MAX_ENV_ENTRIES: usize = 64;
    const MAX_ENV_VALUE_LEN: usize = 64 * 1024;
    // RFE #105 round-3: parallel caps for the labels surface. Docker
    // itself doesn't impose a hard count limit but does fold every
    // label into the container's metadata which the daemon keeps
    // hot; the same 64 / 64 KiB shape we use for env entries is the
    // right defensive floor.
    const MAX_LABEL_ENTRIES: usize = 64;
    const MAX_LABEL_VALUE_LEN: usize = 64 * 1024;

    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid container name".to_string()))?;
    if !validators.valid_name(name) {
        return Err(LauncherError::BadRequest(
            "invalid container name".to_string(),
        ));
    }

    let image = payload
        .get("image")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("image not allowed".to_string()))?;
    if !validators.valid_image(image) {
        return Err(LauncherError::BadRequest("image not allowed".to_string()));
    }

    // network: optional in the payload (post-0.1.4). When the caller (session-broker)
    // does not specify a network, fall back to the launcher's configured default
    // (BOTWORK_LAUNCHER_DEFAULT_NETWORK). This makes the deploy-time topology
    // decision ("which docker network do plugin containers live in?") a property
    // of *where the launcher runs*, not a per-plugin-registry setting — which is
    // the right granularity since plugins don't get to pick their own network.
    let network: String = match payload.get("network") {
        None | Some(Value::Null) => default_network.to_string(),
        Some(Value::String(value)) => {
            if !validators.valid_network(value) {
                return Err(LauncherError::BadRequest(
                    "invalid docker network".to_string(),
                ));
            }
            value.clone()
        }
        Some(_) => {
            return Err(LauncherError::BadRequest(
                "invalid docker network".to_string(),
            ))
        }
    };

    let staging_path = payload
        .get("staging_path")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid staging_path".to_string()))?;
    if !validators.valid_staging_path(staging_path) {
        return Err(LauncherError::BadRequest(
            "invalid staging_path".to_string(),
        ));
    }

    let with_workspace = match payload.get("with_workspace") {
        None => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(LauncherError::BadRequest(
                "invalid with_workspace".to_string(),
            ))
        }
    };
    let (cpu_limit, memory_limit, pids_limit) = match payload.get("resources") {
        None | Some(Value::Null) => (None, None, None),
        Some(Value::Object(resources)) => {
            for key in resources.keys() {
                if key != "cpus" && key != "memory" && key != "pids" {
                    return Err(LauncherError::BadRequest("invalid resources".to_string()));
                }
            }

            let cpu_limit = match resources.get("cpus") {
                None => None,
                Some(Value::String(limit)) if !limit.is_empty() => Some(limit.as_str()),
                _ => {
                    return Err(LauncherError::BadRequest(
                        "invalid resources.cpus".to_string(),
                    ))
                }
            };
            let memory_limit = match resources.get("memory") {
                None => None,
                Some(Value::String(limit)) if !limit.is_empty() => Some(limit.as_str()),
                _ => {
                    return Err(LauncherError::BadRequest(
                        "invalid resources.memory".to_string(),
                    ))
                }
            };
            let pids_limit = match resources.get("pids") {
                None => None,
                Some(Value::Number(value)) => Some(
                    value
                        .as_u64()
                        .filter(|value| *value >= 1 && *value <= u32::MAX as u64)
                        .map(|value| value as u32)
                        .ok_or_else(|| {
                            LauncherError::BadRequest("invalid resources.pids".to_string())
                        })?,
                ),
                _ => {
                    return Err(LauncherError::BadRequest(
                        "invalid resources.pids".to_string(),
                    ))
                }
            };

            (cpu_limit, memory_limit, pids_limit)
        }
        Some(_) => return Err(LauncherError::BadRequest("invalid resources".to_string())),
    };

    let env = match payload.get("env") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) => {
            if entries.len() > MAX_ENV_ENTRIES {
                return Err(LauncherError::PayloadTooLarge(
                    "too many env entries".to_string(),
                ));
            }

            let mut seen = HashSet::new();
            let mut env = Vec::with_capacity(entries.len());
            for entry in entries {
                let Value::Object(entry_obj) = entry else {
                    return Err(LauncherError::BadRequest("invalid env entry".to_string()));
                };
                // The wire contract requires exactly {"name": "...", "value": "..."}.
                if entry_obj.len() != 2 {
                    return Err(LauncherError::BadRequest("invalid env entry".to_string()));
                }

                let name = entry_obj
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LauncherError::BadRequest("invalid env entry".to_string()))?;
                let value = entry_obj
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LauncherError::BadRequest("invalid env entry".to_string()))?;

                if !validators.valid_env_name(name) {
                    return Err(LauncherError::BadRequest(format!(
                        "invalid env name: {name}"
                    )));
                }
                if value.contains('\0') {
                    return Err(LauncherError::BadRequest("invalid env value".to_string()));
                }
                if is_sensitive_env(name) && value.contains('\n') {
                    return Err(LauncherError::BadRequest(
                        "sensitive env value must not contain newline".to_string(),
                    ));
                }
                if value.len() > MAX_ENV_VALUE_LEN {
                    return Err(LauncherError::PayloadTooLarge(
                        "env value too large".to_string(),
                    ));
                }
                if !seen.insert(name) {
                    return Err(LauncherError::BadRequest(format!(
                        "duplicate env name: {name}"
                    )));
                }

                env.push((name.to_string(), value.to_string()));
            }
            env
        }
        Some(_) => return Err(LauncherError::BadRequest("invalid env".to_string())),
    };

    // RFE #105 round-3: docker labels. Optional in the payload to
    // keep older session-broker builds wire-compatible. When present,
    // every entry passes the same `valid_label_name` rule documented
    // in `validate.rs`. Cap counts mirror the env-list caps because
    // docker's own limit is the practical one (an unbounded label
    // list could OOM the daemon's image-id index) and our wire
    // contract should fail fast at the same threshold.
    let labels = match payload.get("labels") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) => {
            if entries.len() > MAX_LABEL_ENTRIES {
                return Err(LauncherError::PayloadTooLarge(
                    "too many label entries".to_string(),
                ));
            }
            let mut seen = HashSet::new();
            let mut labels = Vec::with_capacity(entries.len());
            for entry in entries {
                let Value::Object(entry_obj) = entry else {
                    return Err(LauncherError::BadRequest("invalid label entry".to_string()));
                };
                if entry_obj.len() != 2 {
                    return Err(LauncherError::BadRequest("invalid label entry".to_string()));
                }
                let name = entry_obj
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LauncherError::BadRequest("invalid label entry".to_string()))?;
                let value = entry_obj
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LauncherError::BadRequest("invalid label entry".to_string()))?;
                if !validators.valid_label_name(name) {
                    return Err(LauncherError::BadRequest(format!(
                        "invalid label name: {name}"
                    )));
                }
                if value.contains('\0') || value.contains('\n') || value.contains('\r') {
                    return Err(LauncherError::BadRequest("invalid label value".to_string()));
                }
                if value.len() > MAX_LABEL_VALUE_LEN {
                    return Err(LauncherError::PayloadTooLarge(
                        "label value too large".to_string(),
                    ));
                }
                if !seen.insert(name) {
                    return Err(LauncherError::BadRequest(format!(
                        "duplicate label name: {name}"
                    )));
                }
                labels.push((name.to_string(), value.to_string()));
            }
            labels
        }
        Some(_) => {
            return Err(LauncherError::BadRequest("invalid labels".to_string()));
        }
    };

    Ok(LaunchRequest {
        name,
        image,
        network,
        staging_path,
        with_workspace,
        pids_limit,
        cpu_limit,
        memory_limit,
        env,
        labels,
    })
}

/// Splice the configured egress proxy env vars into the caller-supplied
/// env list. Returns the original list unchanged when `proxy_url` is
/// `None` (the default-off case).
///
/// Wire shape we inject:
///
/// * `HTTPS_PROXY=<proxy_url>` — every HTTP/HTTPS-honouring library
///   we have inside any of the MCP images respects this (curl,
///   requests, urllib3, node fetch).
/// * `HTTP_PROXY=<proxy_url>` — same coverage; some libs split which
///   they honour, so set both. Caps `_PROXY` form is the canonical
///   one for HTTPS_PROXY (some libs deliberately *ignore* `https_proxy`
///   lowercase when running as root); we set caps only.
/// * `NO_PROXY=localhost,127.0.0.1` — the only thing a plugin
///   legitimately reaches via loopback is itself (its own MCP server
///   port). Brokers (config-broker, session-broker, control-plane,
///   auth-broker) are NOT in NO_PROXY because plugins don't talk to
///   them in the supported topology — session-broker talks to the
///   plugin (via the ingress envoy's ext_proc), not the other way
///   round. Adding broker aliases here would invite a future plugin
///   from quietly bypassing the egress proxy to reach a broker if
///   someone wired up a callback path.
///
/// Caller-supplied env wins if it sets one of these names; we never
/// silently shadow an intentional override. The dedupe pass is the
/// `seen` HashSet that mirrors the same shape `parse_launch_payload`
/// uses for duplicate detection. We don't validate `valid_env_name`
/// on the injected names because they are operator-controlled
/// constants in this file, not caller-controlled input.
fn inject_proxy_env(
    mut env: Vec<(String, String)>,
    proxy_url: Option<&str>,
) -> Vec<(String, String)> {
    let Some(proxy_url) = proxy_url else {
        return env;
    };
    let existing: HashSet<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
    let injections: [(&str, String); 3] = [
        ("HTTPS_PROXY", proxy_url.to_string()),
        ("HTTP_PROXY", proxy_url.to_string()),
        ("NO_PROXY", "localhost,127.0.0.1".to_string()),
    ];
    // Build the list of names we still need to insert before we touch
    // `env` so the existing-set borrow goes out of scope cleanly.
    let to_insert: Vec<(&'static str, String)> = injections
        .into_iter()
        .filter(|(name, _)| !existing.contains(*name))
        .collect();
    for (name, value) in to_insert {
        env.push((name.to_string(), value));
    }
    env
}

fn parse_bind_agent_payload<'a>(
    payload: &'a Map<String, Value>,
    validators: &Validators,
) -> Result<BindAgentRequest<'a>, LauncherError> {
    let staging_path = payload
        .get("staging_path")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid staging_path".to_string()))?;
    if !validators.valid_staging_path(staging_path) {
        return Err(LauncherError::BadRequest(
            "invalid staging_path".to_string(),
        ));
    }

    let agent_dir = payload
        .get("agent_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid agent_dir".to_string()))?;
    if !validators.valid_agent_dir(agent_dir) {
        return Err(LauncherError::BadRequest("invalid agent_dir".to_string()));
    }

    Ok(BindAgentRequest {
        staging_path,
        agent_dir,
    })
}

fn parse_teardown_payload<'a>(
    payload: &'a Map<String, Value>,
    validators: &Validators,
) -> Result<TeardownRequest<'a>, LauncherError> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid container name".to_string()))?;
    if !validators.valid_name(name) {
        return Err(LauncherError::BadRequest(
            "invalid container name".to_string(),
        ));
    }

    let staging_path = payload
        .get("staging_path")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid staging_path".to_string()))?;
    if !validators.valid_staging_path(staging_path) {
        return Err(LauncherError::BadRequest(
            "invalid staging_path".to_string(),
        ));
    }

    Ok(TeardownRequest { name, staging_path })
}

fn error_response(error: LauncherError) -> Response<Full<Bytes>> {
    let status =
        StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let message = error.message().to_string();
    log_info(&format!(
        "error_response: status={} message={message}",
        status.as_u16()
    ));
    json_response(status, &["error", message.as_str()])
}

fn json_response(status: StatusCode, fields: &[&str]) -> Response<Full<Bytes>> {
    let body = render_json_object(fields);
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

fn render_json_object(fields: &[&str]) -> String {
    let mut parts = Vec::new();
    let mut index = 0;
    while index + 1 < fields.len() {
        let key = serde_json::to_string(fields[index]).unwrap_or_else(|_| "\"\"".to_string());
        let value = serde_json::to_string(fields[index + 1]).unwrap_or_else(|_| "\"\"".to_string());
        parts.push(format!("{key}: {value}"));
        index += 2;
    }
    format!("{{{}}}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::Request;
    use serde_json::{Map, Value};
    use tempfile::TempDir;

    use super::{
        inject_proxy_env, parse_bind_agent_payload, parse_json_bytes, parse_json_object,
        parse_launch_payload, parse_teardown_payload, render_json_object,
    };
    use crate::error::LauncherError;
    use crate::validate::{Validators, RESERVED_ENV_NAMES};

    fn validators() -> Validators {
        Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators")
    }

    fn valid_launch_payload() -> Map<String, Value> {
        // Deliberately omits `network`: parse_launch_payload falls back to
        // the configured default. Tests that exercise explicit-network handling
        // insert it themselves.
        let mut payload = Map::new();
        payload.insert(
            "name".to_string(),
            Value::String("mcp_session_aabbccddeeff".to_string()),
        );
        payload.insert(
            "image".to_string(),
            Value::String("botwork/mcp-echo:local".to_string()),
        );
        payload.insert(
            "staging_path".to_string(),
            Value::String("/var/lib/botwork/tenants/acme/staging/aabbccddeeff".to_string()),
        );
        payload
    }

    #[test]
    fn response_json_matches_python_spacing() {
        assert_eq!(
            render_json_object(&["name", "mcp_session_aabbccddeeff", "status", "started"]),
            r#"{"name": "mcp_session_aabbccddeeff", "status": "started"}"#
        );
    }

    #[test]
    fn parse_json_bytes_rejects_invalid_json_and_returns_non_object_value() {
        assert!(matches!(
            parse_json_bytes(br#"{"name":"x""#),
            Err(LauncherError::BadRequest(msg)) if msg == "request body must be valid JSON"
        ));

        let parsed = parse_json_bytes(b"[]").expect("json parses");
        assert!(matches!(parsed, Value::Array(_)));
    }

    #[test]
    fn launch_payload_missing_or_wrong_type_fields_are_400_errors() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.remove("staging_path");

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid staging_path"
        ));

        payload.insert(
            "staging_path".to_string(),
            Value::String("/var/lib/botwork/tenants/acme/staging/aabbccddeeff".to_string()),
        );
        payload.insert(
            "with_workspace".to_string(),
            Value::String("false".to_string()),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid with_workspace"
        ));
    }

    #[test]
    fn launch_payload_accepts_env_array() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([
                {"name": "BOTWORK_SECRET_GITHUB_COM_PAT", "value": "ghp_xxx"},
                {"name": "BOTWORK_SECRET_SHARED_SECRET", "value": "another-value"}
            ]),
        );

        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert_eq!(
            parsed.env,
            vec![
                (
                    "BOTWORK_SECRET_GITHUB_COM_PAT".to_string(),
                    "ghp_xxx".to_string()
                ),
                (
                    "BOTWORK_SECRET_SHARED_SECRET".to_string(),
                    "another-value".to_string()
                ),
            ]
        );
    }

    #[test]
    fn launch_payload_env_omitted_is_empty_vec() {
        let validators = validators();
        let payload = valid_launch_payload();

        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert!(parsed.env.is_empty());
    }

    #[test]
    fn launch_payload_env_null_is_empty_vec() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("env".to_string(), Value::Null);

        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert!(parsed.env.is_empty());
    }

    #[test]
    fn launch_payload_env_wrong_type_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            Value::String("BOTWORK_FOO=bar".to_string()),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid env"
        ));
    }

    #[test]
    fn launch_payload_env_entry_missing_fields_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();

        for env in [
            serde_json::json!([{"value": "v"}]),
            serde_json::json!([{"name": "BOTWORK_FOO"}]),
            serde_json::json!([{"name": 1, "value": "v"}]),
            serde_json::json!([{"name": "BOTWORK_FOO", "value": 1}]),
            serde_json::json!([{"name": "BOTWORK_FOO", "value": "v", "extra": "x"}]),
        ] {
            payload.insert("env".to_string(), env);
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid env entry"
            ));
        }
    }

    #[test]
    fn launch_payload_env_invalid_name_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();

        for invalid_name in [
            "botwork_secret",
            "BOTWORK-SECRET",
            "1BOTWORK_SECRET",
            "BOTWORK=SECRET",
            "BOTWORK_\0_SECRET",
        ] {
            payload.insert(
                "env".to_string(),
                serde_json::json!([{"name": invalid_name, "value": "x"}]),
            );
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == format!("invalid env name: {invalid_name}")
            ));
        }

        for reserved in RESERVED_ENV_NAMES {
            payload.insert(
                "env".to_string(),
                serde_json::json!([{"name": reserved, "value": "x"}]),
            );
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == format!("invalid env name: {reserved}")
            ));
        }

        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "DOCKER_FOO", "value": "x"}]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid env name: DOCKER_FOO"
        ));
    }

    #[test]
    fn launch_payload_env_accepts_home_and_user() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([
                {"name": "HOME", "value": "/workspace"},
                {"name": "USER", "value": "botwork"}
            ]),
        );
        let parsed = parse_launch_payload(&payload, &validators, "botwork-test")
            .expect("HOME and USER should be accepted");
        assert_eq!(
            parsed.env,
            vec![
                ("HOME".to_string(), "/workspace".to_string()),
                ("USER".to_string(), "botwork".to_string()),
            ]
        );
    }

    #[test]
    fn launch_payload_env_invalid_value_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "BOTWORK_SECRET", "value": "bad\0value"}]),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid env value"
        ));
    }

    #[test]
    fn launch_payload_sensitive_env_with_newline_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "BOTWORK_SECRET_TOKEN", "value": "line1\nline2"}]),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg))
                if msg == "sensitive env value must not contain newline"
        ));
    }

    #[test]
    fn launch_payload_non_sensitive_env_with_newline_is_accepted() {
        // Non-sensitive values are passed via -e on argv; the env-file format
        // restriction (no newlines) only applies to sensitive vars routed via stdin.
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "BOTWORK_FOO", "value": "line1\nline2"}]),
        );

        let parsed = parse_launch_payload(&payload, &validators, "botwork-test")
            .expect("non-sensitive env with newline should be accepted");
        assert_eq!(parsed.env[0].1, "line1\nline2");
    }

    #[test]
    fn launch_payload_env_duplicate_name_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "env".to_string(),
            serde_json::json!([
                {"name": "BOTWORK_SECRET", "value": "one"},
                {"name": "BOTWORK_SECRET", "value": "two"}
            ]),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "duplicate env name: BOTWORK_SECRET"
        ));
    }

    #[test]
    fn launch_payload_env_too_many_entries_is_413() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        let entries: Vec<Value> = (0..65)
            .map(|i| serde_json::json!({"name": format!("BOTWORK_SECRET_{i}"), "value": "v"}))
            .collect();
        payload.insert("env".to_string(), Value::Array(entries));

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "too many env entries"
        ));
    }

    #[test]
    fn launch_payload_env_value_too_large_is_413() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        let oversized = "x".repeat(64 * 1024 + 1);
        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "BOTWORK_SECRET", "value": oversized}]),
        );

        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "env value too large"
        ));
    }

    #[test]
    fn launch_payload_resources_omitted_or_null_defaults_to_none() {
        let validators = validators();
        let payload = valid_launch_payload();
        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert_eq!(parsed.cpu_limit, None);
        assert_eq!(parsed.memory_limit, None);
        assert_eq!(parsed.pids_limit, None);

        let mut payload = valid_launch_payload();
        payload.insert("resources".to_string(), Value::Null);
        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert_eq!(parsed.cpu_limit, None);
        assert_eq!(parsed.memory_limit, None);
        assert_eq!(parsed.pids_limit, None);
    }

    #[test]
    fn launch_payload_resources_accepts_partial_overrides() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "resources".to_string(),
            serde_json::json!({
                "memory": "4g",
                "pids": 1024
            }),
        );

        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-test").expect("launch payload");
        assert_eq!(parsed.cpu_limit, None);
        assert_eq!(parsed.memory_limit, Some("4g"));
        assert_eq!(parsed.pids_limit, Some(1024));
    }

    #[test]
    fn launch_payload_resources_rejects_invalid_shape_and_unknown_keys() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("resources".to_string(), Value::String("4g".to_string()));
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid resources"
        ));

        payload.insert(
            "resources".to_string(),
            serde_json::json!({
                "memory": "4g",
                "memory_limit": "4g"
            }),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid resources"
        ));
    }

    #[test]
    fn launch_payload_resources_rejects_invalid_fields() {
        let validators = validators();
        let mut payload = valid_launch_payload();

        for resources in [
            serde_json::json!({"cpus": ""}),
            serde_json::json!({"cpus": 1}),
        ] {
            payload.insert("resources".to_string(), resources);
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid resources.cpus"
            ));
        }

        for resources in [
            serde_json::json!({"memory": ""}),
            serde_json::json!({"memory": 1}),
        ] {
            payload.insert("resources".to_string(), resources);
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid resources.memory"
            ));
        }

        for resources in [
            serde_json::json!({"pids": 0}),
            serde_json::json!({"pids": -1}),
            serde_json::json!({"pids": 1.5}),
            serde_json::json!({"pids": "1"}),
        ] {
            payload.insert("resources".to_string(), resources);
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid resources.pids"
            ));
        }
    }

    #[test]
    fn launch_payload_network_falls_back_to_default_when_absent() {
        let validators = validators();
        let payload = valid_launch_payload();
        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-plugin").expect("launch payload");
        assert_eq!(parsed.network, "botwork-plugin");
    }

    #[test]
    fn launch_payload_network_falls_back_to_default_when_null() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("network".to_string(), Value::Null);
        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-plugin").expect("launch payload");
        assert_eq!(parsed.network, "botwork-plugin");
    }

    #[test]
    fn launch_payload_network_explicit_override_wins_over_default() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "network".to_string(),
            Value::String("botwork-override".to_string()),
        );
        let parsed =
            parse_launch_payload(&payload, &validators, "botwork-plugin").expect("launch payload");
        assert_eq!(parsed.network, "botwork-override");
    }

    #[test]
    fn launch_payload_network_invalid_string_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "network".to_string(),
            Value::String("bad network".to_string()),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-plugin"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid docker network"
        ));
    }

    #[test]
    fn launch_payload_network_wrong_type_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("network".to_string(), Value::Number(42.into()));
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-plugin"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid docker network"
        ));
    }

    #[test]
    fn inject_proxy_env_is_noop_when_no_proxy_configured() {
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let out = inject_proxy_env(env.clone(), None);
        assert_eq!(out, env);
    }

    #[test]
    fn inject_proxy_env_adds_three_canonical_vars() {
        let out = inject_proxy_env(Vec::new(), Some("http://egress_envoy:3128"));
        // Order isn't part of the wire contract, sort for deterministic assertion.
        let mut names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"]);
        let map: std::collections::HashMap<&str, &str> =
            out.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        assert_eq!(map["HTTPS_PROXY"], "http://egress_envoy:3128");
        assert_eq!(map["HTTP_PROXY"], "http://egress_envoy:3128");
        assert_eq!(map["NO_PROXY"], "localhost,127.0.0.1");
    }

    #[test]
    fn inject_proxy_env_does_not_clobber_caller_supplied_overrides() {
        // If the caller (today: never; tomorrow: maybe) sets one of the
        // proxy names themselves, their value wins. Documents that the
        // injection is additive, not authoritative.
        let env = vec![
            (
                "HTTPS_PROXY".to_string(),
                "http://caller-supplied:9999".to_string(),
            ),
            ("FOO".to_string(), "bar".to_string()),
        ];
        let out = inject_proxy_env(env, Some("http://egress_envoy:3128"));
        let map: std::collections::HashMap<&str, &str> =
            out.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        assert_eq!(map["HTTPS_PROXY"], "http://caller-supplied:9999");
        // The other two still get injected, since they weren't set.
        assert_eq!(map["HTTP_PROXY"], "http://egress_envoy:3128");
        assert_eq!(map["NO_PROXY"], "localhost,127.0.0.1");
        assert_eq!(map["FOO"], "bar");
        // And we don't double-up HTTPS_PROXY.
        let https_count = out.iter().filter(|(n, _)| n == "HTTPS_PROXY").count();
        assert_eq!(https_count, 1);
    }

    #[test]
    fn inject_proxy_env_preserves_caller_supplied_env_order() {
        let env = vec![
            ("FOO".to_string(), "1".to_string()),
            ("BAR".to_string(), "2".to_string()),
        ];
        let out = inject_proxy_env(env, Some("http://egress_envoy:3128"));
        // First two entries unchanged.
        assert_eq!(out[0], ("FOO".to_string(), "1".to_string()));
        assert_eq!(out[1], ("BAR".to_string(), "2".to_string()));
    }

    #[tokio::test]
    async fn parse_json_object_rejects_bodies_over_limit() {
        let oversized = vec![b'a'; 65_537];
        let request = Request::builder()
            .uri("/launch")
            .body(Full::new(Bytes::from(oversized)))
            .expect("request");

        assert!(matches!(
            parse_json_object(request).await,
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "request body exceeds 65536 bytes"
        ));
    }

    // ── RFE #105 round-3: launch payload labels ─────────────────────────────
    //
    // The wire contract is documented on the `LaunchRequest::labels`
    // field and the `validate.rs::valid_label_name` validator; these
    // tests pin it as observable behaviour.
    //
    // session-broker (round-3 PR2) will pass three entries every
    // spawn: `io.botworkz.tenant`, `io.botworkz.workspace`,
    // `io.botworkz.plugin`. The "older callers (no labels field)"
    // case must keep working unchanged — that's the whole reason
    // this PR ships as a standalone wire-additive change ahead of
    // the broker-side rewrite.
    //
    // (`io.botworkz.tenant` is what an operator will eventually
    // `docker ps --filter label=…` to bisect a stuck session.)

    #[test]
    fn launch_payload_omits_labels_field_is_empty_slice() {
        // Wire compat: today's session-broker never sets `labels`;
        // the launcher must default it to empty and behave identically
        // to the pre-PR shape. Without this, every spawn from the
        // legacy broker would 400 the moment the launcher images
        // bump.
        let validators = validators();
        let payload = valid_launch_payload();
        let parsed = parse_launch_payload(&payload, &validators, "botwork-test")
            .expect("payload without labels must parse");
        assert!(parsed.labels.is_empty());
    }

    #[test]
    fn launch_payload_labels_null_is_empty_slice() {
        // Parity with `env: null` and `network: null` — JSON null is
        // treated as "omitted" rather than rejected.
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("labels".to_string(), Value::Null);
        let parsed = parse_launch_payload(&payload, &validators, "botwork-test")
            .expect("labels: null must parse");
        assert!(parsed.labels.is_empty());
    }

    #[test]
    fn launch_payload_accepts_labels_array_in_declaration_order() {
        // session-broker's labels arrive in a stable order
        // (tenant, workspace, plugin); the launcher must round-trip
        // that order so the eventual `docker ps --format`-readable
        // output is predictable for operator + janitor scripts.
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "labels".to_string(),
            serde_json::json!([
                {"name": "io.botworkz.tenant",    "value": "acme"},
                {"name": "io.botworkz.workspace", "value": "mcp"},
                {"name": "io.botworkz.plugin",    "value": "mcp-bash"},
            ]),
        );
        let parsed = parse_launch_payload(&payload, &validators, "botwork-test")
            .expect("well-formed labels must parse");
        assert_eq!(
            parsed.labels,
            vec![
                ("io.botworkz.tenant".to_string(), "acme".to_string()),
                ("io.botworkz.workspace".to_string(), "mcp".to_string()),
                ("io.botworkz.plugin".to_string(), "mcp-bash".to_string()),
            ]
        );
    }

    #[test]
    fn launch_payload_label_outside_namespace_is_400() {
        // Belt + braces with the validator's own tests: the
        // wire-side rejection must produce a clean 400 with the
        // operator-readable message (not a panic, not a 500).
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "labels".to_string(),
            serde_json::json!([
                {"name": "tenant", "value": "acme"},
            ]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid label name: tenant"
        ));
    }

    #[test]
    fn launch_payload_label_entry_must_be_name_value_object() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        // Bare-string entry instead of {name,value}.
        payload.insert(
            "labels".to_string(),
            serde_json::json!(["io.botworkz.tenant=acme"]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid label entry"
        ));
        // Extra keys beyond name/value.
        payload.insert(
            "labels".to_string(),
            serde_json::json!([
                {"name": "io.botworkz.tenant", "value": "acme", "extra": "no"},
            ]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid label entry"
        ));
    }

    #[test]
    fn launch_payload_label_value_with_newline_or_null_is_400() {
        // docker.rs writes `--label key=value` directly to argv;
        // a newline in the value would break docker's KEY=VALUE
        // parser, and a null byte is a UTF-8 hazard. Both refused
        // at the wire boundary so docker.rs can treat the slice
        // as trusted.
        let validators = validators();
        for bad in ["nl\nhere", "nul\0byte", "cr\rhere"] {
            let mut payload = valid_launch_payload();
            payload.insert(
                "labels".to_string(),
                serde_json::json!([{"name": "io.botworkz.tenant", "value": bad}]),
            );
            assert!(matches!(
                parse_launch_payload(&payload, &validators, "botwork-test"),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid label value",
            ));
        }
    }

    #[test]
    fn launch_payload_label_value_too_large_is_413() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        let oversize = "a".repeat(64 * 1024 + 1);
        payload.insert(
            "labels".to_string(),
            serde_json::json!([{"name": "io.botworkz.tenant", "value": oversize}]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "label value too large"
        ));
    }

    #[test]
    fn launch_payload_duplicate_label_names_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert(
            "labels".to_string(),
            serde_json::json!([
                {"name": "io.botworkz.tenant", "value": "acme"},
                {"name": "io.botworkz.tenant", "value": "other"},
            ]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "duplicate label name: io.botworkz.tenant"
        ));
    }

    #[test]
    fn launch_payload_too_many_label_entries_is_413() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        let entries: Vec<Value> = (0..65)
            .map(|i| serde_json::json!({"name": format!("io.botworkz.k{i}"), "value": "v"}))
            .collect();
        payload.insert("labels".to_string(), Value::Array(entries));
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "too many label entries"
        ));
    }

    #[test]
    fn launch_payload_labels_wrong_type_is_400() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("labels".to_string(), Value::String("invalid".to_string()));
        assert!(matches!(
            parse_launch_payload(&payload, &validators, "botwork-test"),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid labels"
        ));
    }

    // ── render_json_object edge cases ─────────────────────────────────────────

    #[test]
    fn render_json_object_empty_fields_produces_empty_object() {
        assert_eq!(render_json_object(&[]), "{}");
    }

    #[test]
    fn render_json_object_odd_count_drops_trailing_unpaired_key() {
        // Three elements: only the first pair "a":"b" is emitted;
        // the dangling "c" is silently dropped.
        let out = render_json_object(&["a", "b", "c"]);
        assert_eq!(out, r#"{"a": "b"}"#);
    }

    // ── parse_json_bytes edge cases ───────────────────────────────────────────

    #[test]
    fn parse_json_bytes_empty_body_returns_empty_object() {
        let v = parse_json_bytes(b"").expect("empty body must succeed");
        assert!(matches!(v, Value::Object(ref m) if m.is_empty()));
    }

    // ── parse_bind_agent_payload ──────────────────────────────────────────────

    fn valid_bind_agent_payload(base: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "staging_path".to_string(),
            Value::String(format!("{base}/acme/staging/aabbccddeeff")),
        );
        m.insert(
            "agent_dir".to_string(),
            Value::String(format!("{base}/acme/workspaces/ws/agents/agent-1")),
        );
        m
    }

    fn valid_teardown_payload(base: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "name".to_string(),
            Value::String("mcp_session_aabbccddeeff".to_string()),
        );
        m.insert(
            "staging_path".to_string(),
            Value::String(format!("{base}/acme/staging/aabbccddeeff")),
        );
        m
    }

    #[test]
    fn parse_bind_agent_missing_staging_path_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_bind_agent_payload(&base);
        payload.remove("staging_path");
        let err = parse_bind_agent_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_bind_agent_invalid_staging_path_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_bind_agent_payload(&base);
        payload.insert(
            "staging_path".to_string(),
            Value::String("/outside/staging/abc".to_string()),
        );
        let err = parse_bind_agent_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_bind_agent_missing_agent_dir_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_bind_agent_payload(&base);
        payload.remove("agent_dir");
        let err = parse_bind_agent_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_bind_agent_invalid_agent_dir_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_bind_agent_payload(&base);
        payload.insert(
            "agent_dir".to_string(),
            Value::String("/outside/agents/agent-1".to_string()),
        );
        let err = parse_bind_agent_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    // ── parse_teardown_payload ────────────────────────────────────────────────

    #[test]
    fn parse_teardown_missing_name_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_teardown_payload(&base);
        payload.remove("name");
        let err = parse_teardown_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_teardown_invalid_name_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_teardown_payload(&base);
        payload.insert(
            "name".to_string(),
            Value::String("INVALID_CONTAINER_NAME_!!!".to_string()),
        );
        let err = parse_teardown_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_teardown_missing_staging_path_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_teardown_payload(&base);
        payload.remove("staging_path");
        let err = parse_teardown_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }

    #[test]
    fn parse_teardown_invalid_staging_path_returns_bad_request() {
        let temp_dir = TempDir::new().expect("tempdir");
        let base = temp_dir.path().to_string_lossy().to_string();
        let validators =
            Validators::new_with_bases(r"^botwork/.*$", &base, &base).expect("validators");
        let mut payload = valid_teardown_payload(&base);
        payload.insert(
            "staging_path".to_string(),
            Value::String("/outside/staging/abc".to_string()),
        );
        let err = parse_teardown_payload(&payload, &validators).expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }
}
