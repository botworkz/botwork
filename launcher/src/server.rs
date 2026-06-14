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
use crate::validate::Validators;

const MAX_JSON_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub validators: Validators,
}

struct LaunchRequest<'a> {
    name: &'a str,
    image: &'a str,
    network: &'a str,
    staging_path: &'a str,
    with_workspace: bool,
    pids_limit: Option<u32>,
    cpu_limit: Option<&'a str>,
    memory_limit: Option<&'a str>,
    env: Vec<(String, String)>,
}

struct BindAgentRequest<'a> {
    staging_path: &'a str,
    agent_dir: &'a str,
}

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
    let launch = parse_launch_payload(&payload, &state.validators)?;

    let status = docker::ensure_container(
        &ContainerLaunch {
            name: launch.name,
            image: launch.image,
            network: launch.network,
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
            env: &launch.env,
        },
        &state.validators,
    )?;

    log_info(&format!(
        "launch ok: name={} image={} network={} staging_path={} env_count={}",
        launch.name,
        launch.image,
        launch.network,
        launch.staging_path,
        launch.env.len()
    ));

    Ok(json_response(
        StatusCode::OK,
        &["name", launch.name, "status", status],
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
) -> Result<LaunchRequest<'a>, LauncherError> {
    const MAX_ENV_ENTRIES: usize = 64;
    const MAX_ENV_VALUE_LEN: usize = 64 * 1024;

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

    let network = payload
        .get("network")
        .and_then(Value::as_str)
        .ok_or_else(|| LauncherError::BadRequest("invalid docker network".to_string()))?;
    if !validators.valid_network(network) {
        return Err(LauncherError::BadRequest(
            "invalid docker network".to_string(),
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
    })
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

    use super::{parse_json_bytes, parse_json_object, parse_launch_payload, render_json_object};
    use crate::error::LauncherError;
    use crate::validate::{Validators, RESERVED_ENV_NAMES};

    fn validators() -> Validators {
        Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators")
    }

    fn valid_launch_payload() -> Map<String, Value> {
        let mut payload = Map::new();
        payload.insert(
            "name".to_string(),
            Value::String("mcp_session_aabbccddeeff".to_string()),
        );
        payload.insert(
            "image".to_string(),
            Value::String("botwork/mcp-echo:local".to_string()),
        );
        payload.insert("network".to_string(), Value::String("botwork".to_string()));
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
            parse_launch_payload(&payload, &validators),
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
            parse_launch_payload(&payload, &validators),
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

        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
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

        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
        assert!(parsed.env.is_empty());
    }

    #[test]
    fn launch_payload_env_null_is_empty_vec() {
        let validators = validators();
        let mut payload = valid_launch_payload();
        payload.insert("env".to_string(), Value::Null);

        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
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
            parse_launch_payload(&payload, &validators),
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
                parse_launch_payload(&payload, &validators),
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
                parse_launch_payload(&payload, &validators),
                Err(LauncherError::BadRequest(msg)) if msg == format!("invalid env name: {invalid_name}")
            ));
        }

        for reserved in RESERVED_ENV_NAMES {
            payload.insert(
                "env".to_string(),
                serde_json::json!([{"name": reserved, "value": "x"}]),
            );
            assert!(matches!(
                parse_launch_payload(&payload, &validators),
                Err(LauncherError::BadRequest(msg)) if msg == format!("invalid env name: {reserved}")
            ));
        }

        payload.insert(
            "env".to_string(),
            serde_json::json!([{"name": "DOCKER_FOO", "value": "x"}]),
        );
        assert!(matches!(
            parse_launch_payload(&payload, &validators),
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
        let parsed =
            parse_launch_payload(&payload, &validators).expect("HOME and USER should be accepted");
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
            parse_launch_payload(&payload, &validators),
            Err(LauncherError::BadRequest(msg)) if msg == "invalid env value"
        ));
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
            parse_launch_payload(&payload, &validators),
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
            parse_launch_payload(&payload, &validators),
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
            parse_launch_payload(&payload, &validators),
            Err(LauncherError::PayloadTooLarge(msg)) if msg == "env value too large"
        ));
    }

    #[test]
    fn launch_payload_resources_omitted_or_null_defaults_to_none() {
        let validators = validators();
        let payload = valid_launch_payload();
        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
        assert_eq!(parsed.cpu_limit, None);
        assert_eq!(parsed.memory_limit, None);
        assert_eq!(parsed.pids_limit, None);

        let mut payload = valid_launch_payload();
        payload.insert("resources".to_string(), Value::Null);
        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
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

        let parsed = parse_launch_payload(&payload, &validators).expect("launch payload");
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
            parse_launch_payload(&payload, &validators),
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
            parse_launch_payload(&payload, &validators),
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
                parse_launch_payload(&payload, &validators),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid resources.cpus"
            ));
        }

        for resources in [
            serde_json::json!({"memory": ""}),
            serde_json::json!({"memory": 1}),
        ] {
            payload.insert("resources".to_string(), resources);
            assert!(matches!(
                parse_launch_payload(&payload, &validators),
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
                parse_launch_payload(&payload, &validators),
                Err(LauncherError::BadRequest(msg)) if msg == "invalid resources.pids"
            ));
        }
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
}
