//! Thin api client used by `botctl bootstrap`.
//!
//! Why a separate client (vs reusing the `reqwest` dance inline in
//! `apply.rs`): the bootstrap subcommand needs to:
//!
//! 1. List + read entities to compute the diff (what's already there
//!    vs what the yaml wants);
//! 2. Create new rows;
//! 3. Update changed rows;
//! 4. Carry the appropriate auth headers on every request.
//!
//! Wrapping the HTTP shape behind a typed surface keeps `apply.rs`
//! readable and makes the wiremock-stubbed tests in
//! `tests/bootstrap_apply_test.rs` use real wire calls — same
//! integration shape session-broker and api use against
//! control-plane (botworkz/botwork#92, #112).
//!
//! # Wire contract (matches api/src/{read,write}.rs — Phase 2, space#311)
//!
//! ```text
//! # Admin-gated (x-botwork-admin: <operator> required)
//! GET    /api/tenants                                   -> { items, total }
//! POST   /api/tenants                                   {name}
//! PUT    /api/tenants/{id}                              {name, if_unmodified_since}
//!
//! GET    /api/plugins                                   -> { items, total }
//! POST   /api/plugins                                   {name, image, port, …, egress}
//! PUT    /api/plugins/{id}                              {name, image, …, if_unmodified_since}
//!
//! # Tenant-scoped (x-botwork-tenant: <name> required, must match path)
//! GET    /api/tenant/{tenant}/workspaces                -> { items, total }
//! POST   /api/tenant/{tenant}/workspaces                {name}
//! PUT    /api/tenant/{tenant}/workspaces/{id}           {name, if_unmodified_since}
//!
//! GET    /api/tenant/{tenant}/workspace_plugins         -> { items, total }
//!        ?workspace_id=<uuid>&plugin_id=<uuid>
//! POST   /api/tenant/{tenant}/workspace_plugins         {workspace_id, plugin_id, config?}
//! PUT    /api/tenant/{tenant}/workspace_plugins/{wid}/{pid}
//!                                                       {config?, if_unmodified_since}
//! ```
//!
//! ## Header conventions
//!
//! * **Admin-gated routes** (`/api/tenants`, `/api/plugins` and their
//!   sub-paths): `x-botwork-admin: <operator>`. The API requires the
//!   header to be present and non-empty; it also reads the value as
//!   the operator identity for audit logs — sending the operator name
//!   satisfies both the auth gate and the audit requirement in one
//!   header.
//! * **Tenant-scoped routes** (`/api/tenant/{tenant}/…`):
//!   `x-botwork-tenant: <tenant>` (must match the path segment).
//!   `x-botwork-admin: <operator>` is also sent so the audit log
//!   records the import operator rather than "anonymous" (the API
//!   reads it from `x-botwork-admin` for all write paths).

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
    /// (e.g. `http://admin_api:9400`). `operator` is sent as the
    /// `x-botwork-admin` header value so api's audit log records
    /// the import operator on every write.
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

    /// URL for admin-gated routes: `{endpoint}/api{path}`.
    fn admin_url(&self, path: &str) -> String {
        format!("{}/api{path}", self.endpoint)
    }

    /// URL for tenant-scoped routes: `{endpoint}/api/tenant/{tenant}{path}`.
    fn tenant_url(&self, tenant: &str, path: &str) -> String {
        format!("{}/api/tenant/{tenant}{path}", self.endpoint)
    }

    pub fn list_tenants(&self) -> Result<Vec<Tenant>, ClientError> {
        self.admin_get_list("/tenants")
    }

    pub fn list_workspaces(&self, tenant: &str) -> Result<Vec<Workspace>, ClientError> {
        self.tenant_get_list(tenant, "/workspaces")
    }

    pub fn list_plugins(&self) -> Result<Vec<Plugin>, ClientError> {
        self.admin_get_list("/plugins")
    }

    pub fn list_workspace_plugins(
        &self,
        tenant: &str,
        workspace_id: Uuid,
    ) -> Result<Vec<WorkspacePlugin>, ClientError> {
        self.tenant_get_list(
            tenant,
            &format!("/workspace_plugins?workspace_id={workspace_id}"),
        )
    }

    pub fn create_tenant(&self, body: &CreateTenant<'_>) -> Result<Tenant, ClientError> {
        self.admin_post("/tenants", body)
    }

    pub fn update_tenant(&self, id: Uuid, body: &UpdateTenant<'_>) -> Result<Tenant, ClientError> {
        self.admin_put(&format!("/tenants/{id}"), body)
    }

    pub fn create_workspace(
        &self,
        tenant: &str,
        body: &CreateWorkspace<'_>,
    ) -> Result<Workspace, ClientError> {
        self.tenant_post(tenant, "/workspaces", body)
    }

    pub fn update_workspace(
        &self,
        tenant: &str,
        id: Uuid,
        body: &UpdateWorkspace<'_>,
    ) -> Result<Workspace, ClientError> {
        self.tenant_put(tenant, &format!("/workspaces/{id}"), body)
    }

    pub fn create_plugin(&self, body: &serde_json::Value) -> Result<Plugin, ClientError> {
        self.admin_post("/plugins", body)
    }

    pub fn update_plugin(&self, id: Uuid, body: &serde_json::Value) -> Result<Plugin, ClientError> {
        self.admin_put(&format!("/plugins/{id}"), body)
    }

    pub fn create_workspace_plugin(
        &self,
        tenant: &str,
        body: &CreateWorkspacePlugin,
    ) -> Result<WorkspacePlugin, ClientError> {
        self.tenant_post(tenant, "/workspace_plugins", body)
    }

    pub fn update_workspace_plugin(
        &self,
        tenant: &str,
        workspace_id: Uuid,
        plugin_id: Uuid,
        body: &UpdateWorkspacePlugin,
    ) -> Result<WorkspacePlugin, ClientError> {
        self.tenant_put(
            tenant,
            &format!("/workspace_plugins/{workspace_id}/{plugin_id}"),
            body,
        )
    }

    // -- internals ------------------------------------------------------

    /// GET list from an admin-gated route. Sends `x-botwork-admin: <operator>`.
    fn admin_get_list<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, ClientError> {
        let url = self.admin_url(path);
        let resp = self
            .http
            .get(&url)
            .header("x-botwork-admin", &self.operator)
            .send()
            .map_err(transport(&url, "GET"))?;
        check_status(&resp.status(), &resp, &url, "GET")?;
        let envelope: ListEnvelope<T> = resp
            .json()
            .map_err(|err| ClientError::Decode(format!("GET {url}: {err}")))?;
        Ok(envelope.items)
    }

    /// GET list from a tenant-scoped route. Sends `x-botwork-tenant: <tenant>`.
    fn tenant_get_list<T: serde::de::DeserializeOwned>(
        &self,
        tenant: &str,
        path: &str,
    ) -> Result<Vec<T>, ClientError> {
        let url = self.tenant_url(tenant, path);
        let resp = self
            .http
            .get(&url)
            .header("x-botwork-tenant", tenant)
            .send()
            .map_err(transport(&url, "GET"))?;
        check_status(&resp.status(), &resp, &url, "GET")?;
        let envelope: ListEnvelope<T> = resp
            .json()
            .map_err(|err| ClientError::Decode(format!("GET {url}: {err}")))?;
        Ok(envelope.items)
    }

    /// POST to an admin-gated route. Sends `x-botwork-admin: <operator>`.
    fn admin_post<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.admin_url(path);
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

    /// POST to a tenant-scoped route. Sends `x-botwork-tenant: <tenant>`
    /// and `x-botwork-admin: <operator>` (for audit log identity).
    fn tenant_post<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        tenant: &str,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.tenant_url(tenant, path);
        let resp = self
            .http
            .post(&url)
            .header("x-botwork-tenant", tenant)
            .header("x-botwork-admin", &self.operator)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .map_err(transport(&url, "POST"))?;
        check_status(&resp.status(), &resp, &url, "POST")?;
        resp.json()
            .map_err(|err| ClientError::Decode(format!("POST {url}: {err}")))
    }

    /// PUT to an admin-gated route. Sends `x-botwork-admin: <operator>`.
    fn admin_put<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.admin_url(path);
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

    /// PUT to a tenant-scoped route. Sends `x-botwork-tenant: <tenant>`
    /// and `x-botwork-admin: <operator>` (for audit log identity).
    fn tenant_put<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        tenant: &str,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = self.tenant_url(tenant, path);
        let resp = self
            .http
            .put(&url)
            .header("x-botwork-tenant", tenant)
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
