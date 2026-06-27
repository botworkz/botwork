//! Thin api client used by `botwork-tools bootstrap`.
//!
//! Why a separate client (vs reusing the `reqwest` dance inline in
//! `apply.rs`): the bootstrap subcommand needs to:
//!
//! 1. List + read entities to compute the diff (what's already there
//!    vs what the yaml wants);
//! 2. Create new rows;
//! 3. Update changed rows;
//! 4. Carry the `x-botwork-admin` operator header on every write.
//!
//! Wrapping the HTTP shape behind a typed surface keeps `apply.rs`
//! readable and makes the wiremock-stubbed tests in
//! `tests/bootstrap_apply_test.rs` use real wire calls — same
//! integration shape session-broker and api use against
//! control-plane (botworkz/botwork#92, #112).
//!
//! # Wire contract (matches api/src/{read,write}.rs)
//!
//! ```text
//! GET    /admin/api/v1/tenants                          -> { items, total }
//! POST   /admin/api/v1/tenants                          {name}
//! PUT    /admin/api/v1/tenants/{id}                     {name, if_unmodified_since}
//!
//! GET    /admin/api/v1/workspaces?tenant_id=<uuid>      -> { items, total }
//! POST   /admin/api/v1/workspaces                       {tenant_id, name}
//! PUT    /admin/api/v1/workspaces/{id}                  {name, if_unmodified_since}
//!
//! GET    /admin/api/v1/plugins                          -> { items, total }
//! POST   /admin/api/v1/plugins                          {name, image, port, …, egress}
//! PUT    /admin/api/v1/plugins/{id}                     {name, image, …, if_unmodified_since}
//!
//! GET    /admin/api/v1/workspace_plugins                -> { items, total }
//!        ?workspace_id=<uuid>&plugin_id=<uuid>
//! POST   /admin/api/v1/workspace_plugins                {workspace_id, plugin_id, config?}
//! PUT    /admin/api/v1/workspace_plugins/{wid}/{pid}    {config?, if_unmodified_since}
//! ```

use std::time::Duration;

use reqwest::blocking::Client as HttpClient;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// api thin client. Cloneable; the underlying reqwest pool is
/// shared. The bootstrap subcommand is serial so a single client
/// suffices, but `apply.rs` constructs one and threads it through —
/// the type just keeps the endpoint + headers in one place.
#[derive(Debug)]
pub struct AdminClient {
    http: HttpClient,
    endpoint: String,
    operator: String,
}

impl AdminClient {
    /// Build a client. `endpoint` should be the api base URL
    /// (e.g. `http://admin_api:9400`); `/admin/api/v1` is appended
    /// per call. `operator` becomes the `x-botwork-admin` header on
    /// every write so api's audit log can distinguish
    /// machine-driven imports from operator UI writes.
    pub fn new(endpoint: &str, operator: &str) -> Result<Self, ClientError> {
        let http = HttpClient::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|err| ClientError::BuildClient(err.to_string()))?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            operator: operator.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/admin/api/v1{path}", self.endpoint)
    }

    pub fn list_tenants(&self) -> Result<Vec<Tenant>, ClientError> {
        self.get_list("/tenants")
    }

    pub fn list_workspaces(&self, tenant_id: Uuid) -> Result<Vec<Workspace>, ClientError> {
        self.get_list(&format!("/workspaces?tenant_id={tenant_id}"))
    }

    pub fn list_plugins(&self) -> Result<Vec<Plugin>, ClientError> {
        self.get_list("/plugins")
    }

    pub fn list_workspace_plugins(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<WorkspacePlugin>, ClientError> {
        self.get_list(&format!("/workspace_plugins?workspace_id={workspace_id}"))
    }

    pub fn create_tenant(&self, body: &CreateTenant<'_>) -> Result<Tenant, ClientError> {
        self.post("/tenants", body)
    }

    pub fn update_tenant(&self, id: Uuid, body: &UpdateTenant<'_>) -> Result<Tenant, ClientError> {
        self.put(&format!("/tenants/{id}"), body)
    }

    pub fn create_workspace(&self, body: &CreateWorkspace<'_>) -> Result<Workspace, ClientError> {
        self.post("/workspaces", body)
    }

    pub fn update_workspace(
        &self,
        id: Uuid,
        body: &UpdateWorkspace<'_>,
    ) -> Result<Workspace, ClientError> {
        self.put(&format!("/workspaces/{id}"), body)
    }

    pub fn create_plugin(&self, body: &serde_json::Value) -> Result<Plugin, ClientError> {
        self.post("/plugins", body)
    }

    pub fn update_plugin(&self, id: Uuid, body: &serde_json::Value) -> Result<Plugin, ClientError> {
        self.put(&format!("/plugins/{id}"), body)
    }

    pub fn create_workspace_plugin(
        &self,
        body: &CreateWorkspacePlugin,
    ) -> Result<WorkspacePlugin, ClientError> {
        self.post("/workspace_plugins", body)
    }

    pub fn update_workspace_plugin(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
        body: &UpdateWorkspacePlugin,
    ) -> Result<WorkspacePlugin, ClientError> {
        self.put(
            &format!("/workspace_plugins/{workspace_id}/{plugin_id}"),
            body,
        )
    }

    // -- internals ------------------------------------------------------

    fn get_list<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, ClientError> {
        let url = self.url(path);
        let resp = self.http.get(&url).send().map_err(transport(&url, "GET"))?;
        check_status(&resp.status(), &resp, &url, "GET")?;
        let envelope: ListEnvelope<T> = resp
            .json()
            .map_err(|err| ClientError::Decode(format!("GET {url}: {err}")))?;
        Ok(envelope.items)
    }

    fn post<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.url(path);
        let resp = self
            .http
            .post(&url)
            .header("x-botwork-admin", &self.operator)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .map_err(transport(&url, "POST"))?;
        check_status(&resp.status(), &resp, &url, "POST")?;
        resp.json()
            .map_err(|err| ClientError::Decode(format!("POST {url}: {err}")))
    }

    fn put<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.url(path);
        let resp = self
            .http
            .put(&url)
            .header("x-botwork-admin", &self.operator)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .map_err(transport(&url, "PUT"))?;
        check_status(&resp.status(), &resp, &url, "PUT")?;
        resp.json()
            .map_err(|err| ClientError::Decode(format!("PUT {url}: {err}")))
    }
}

fn transport(url: &str, verb: &str) -> impl FnOnce(reqwest::Error) -> ClientError + use<> {
    let url = url.to_string();
    let verb = verb.to_string();
    move |err| ClientError::Transport(format!("{verb} {url}: {err}"))
}

fn check_status(
    status: &reqwest::StatusCode,
    resp: &reqwest::blocking::Response,
    url: &str,
    verb: &str,
) -> Result<(), ClientError> {
    if status.is_success() {
        return Ok(());
    }
    // reqwest::blocking::Response can't be consumed twice; the caller
    // still wants .json() on the success path. We only get here on
    // failure so consuming the response is fine — except we already
    // hold a &Response. Read headers, surface the status + url; the
    // detail body is logged on the wire but we keep the error
    // surface minimal here.
    let kind = if status.is_server_error() || *status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        ClientError::Transport(format!("{verb} {url}: server returned {status} (api side)",))
    } else {
        ClientError::Http {
            url: url.to_string(),
            verb: verb.to_string(),
            status: status.as_u16(),
            // body: drained by the caller's failure path; keep this struct lean.
            content_length: resp.content_length(),
        }
    };
    Err(kind)
}

#[derive(Debug, Deserialize)]
struct ListEnvelope<T> {
    items: Vec<T>,
    #[allow(dead_code)]
    total: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    pub id: Uuid,
    pub name: String,
    pub image: String,
    pub port: i32,
    pub path: String,
    pub upstream_auth: String,
    pub env: serde_json::Value,
    pub resources: Option<serde_json::Value>,
    pub egress: serde_json::Value,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspacePlugin {
    pub workspace_id: Uuid,
    pub plugin_id: Uuid,
    pub config: Option<serde_json::Value>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct CreateTenant<'a> {
    pub name: &'a str,
}

#[derive(Debug, Serialize)]
pub struct UpdateTenant<'a> {
    pub name: &'a str,
    pub if_unmodified_since: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct CreateWorkspace<'a> {
    pub tenant_id: Uuid,
    pub name: &'a str,
}

#[derive(Debug, Serialize)]
pub struct UpdateWorkspace<'a> {
    pub name: &'a str,
    pub if_unmodified_since: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct CreateWorkspacePlugin {
    pub workspace_id: Uuid,
    pub plugin_id: Uuid,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct UpdateWorkspacePlugin {
    pub config: Option<serde_json::Value>,
    pub if_unmodified_since: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("failed to build HTTP client: {0}")]
    BuildClient(String),

    /// Transport / server-side failure. Maps to exit 7.
    /// session-broker / api both treat 5xx + connect failures
    /// the same way — "the upstream is broken; abort and surface
    /// loudly".
    #[error("api transport: {0}")]
    Transport(String),

    /// 4xx response. Maps to exit 6 — the data we sent was rejected,
    /// not a transport problem.
    #[error("{verb} {url} -> {status}")]
    Http {
        url: String,
        verb: String,
        status: u16,
        content_length: Option<u64>,
    },

    #[error("decode error: {0}")]
    Decode(String),
}
