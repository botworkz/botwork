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
            pids_limit: state.config.container_pids_limit,
            memory_limit: &state.config.container_memory_limit,
            read_only_rootfs: state.config.container_read_only_rootfs,
        },
        &state.validators,
    )?;

    log_info(&format!(
        "launch ok: name={} image={} network={} staging_path={}",
        launch.name, launch.image, launch.network, launch.staging_path
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

    Ok(LaunchRequest {
        name,
        image,
        network,
        staging_path,
        with_workspace,
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
    use crate::validate::Validators;

    fn validators() -> Validators {
        Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators")
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
