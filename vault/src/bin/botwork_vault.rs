use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing::info;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

use botwork_vault::{PublicStore, SecretEntry, SecretKey, SecretKind, Vault, VaultError};

const PREFIX: &str = "[vault]";
const DEFAULT_SERVER_ENV: &str = "BOTWORK_LOGIN_SERVER";
const BEARER_ENV: &str = "BOTWORK_BEARER";
const SSL_CERT_FILE_ENV: &str = "SSL_CERT_FILE";
const DEFAULT_SERVER: &str = "http://127.0.0.1:9100";
const VERSION: &str = include_str!("../../../VERSION").trim_ascii();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitCode {
    Usage = 2,
    Conflict = 3,
    UnsupportedFormat = 4,
}

impl ExitCode {
    fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CliFailure {
    exit_code: ExitCode,
    message: String,
}

impl CliFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit_code: ExitCode::Usage,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            exit_code: ExitCode::Conflict,
            message: message.into(),
        }
    }

    fn unsupported_format() -> Self {
        Self {
            exit_code: ExitCode::UnsupportedFormat,
            message: "unsupported vault format".to_string(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StdoutPayload {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CommandOutcome {
    stdout: Option<StdoutPayload>,
    stderr: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SanitizedValue {
    value: Vec<u8>,
    warning: Option<String>,
}

struct PreparedPutInput {
    kind: SecretKind,
    sanitized: SanitizedValue,
}

fn emit_failure(err: CliFailure) -> ! {
    eprintln!("ERROR: {}", err.message);
    process::exit(err.exit_code.as_i32());
}

fn emit_outcome(outcome: CommandOutcome) -> Result<(), CliFailure> {
    for line in outcome.stderr {
        eprintln!("{line}");
    }
    if let Some(stdout) = outcome.stdout {
        match stdout {
            StdoutPayload::Text(text) => print!("{text}"),
            StdoutPayload::Bytes(bytes) => std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| CliFailure::usage(e.to_string()))?,
        }
    }
    Ok(())
}

/// Resolve the auth-broker base URL: `--server` > env > built-in.
fn resolve_server(cli_server: Option<&str>) -> String {
    if let Some(value) = cli_server {
        return value.trim().to_string();
    }
    if let Ok(value) = std::env::var(DEFAULT_SERVER_ENV) {
        if !value.is_empty() {
            return value;
        }
    }
    DEFAULT_SERVER.to_string()
}

/// Resolve the optional CA bundle path: `--cacert` > `SSL_CERT_FILE` > none.
fn resolve_cacert(cli_cacert: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = cli_cacert {
        return Some(path.to_path_buf());
    }
    match std::env::var(SSL_CERT_FILE_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Resolve the bearer token. The recommended path is `$BOTWORK_BEARER`
/// (populated by `eval "$(bw env --tenant <t>)"`). Pass `--bearer-stdin`
/// to pipe the bearer without argv exposure.
fn resolve_bearer(bearer_stdin: bool) -> Result<Zeroizing<String>, CliFailure> {
    let mut stdin = std::io::stdin().lock();
    resolve_bearer_from_sources(bearer_stdin, &mut stdin).map_err(CliFailure::usage)
}

fn resolve_bearer_from_sources<R: std::io::Read>(
    bearer_stdin: bool,
    reader: &mut R,
) -> Result<Zeroizing<String>, String> {
    if bearer_stdin {
        let mut buf = Vec::new();
        reader
            .read_to_end_via_buf(&mut buf)
            .map_err(|e| format!("failed to read bearer from stdin: {e}"))?;
        let mut bearer = String::from_utf8(buf)
            .map_err(|e| format!("failed to decode bearer from stdin as UTF-8: {e}"))?;
        if bearer.ends_with('\n') {
            bearer.pop();
            if bearer.ends_with('\r') {
                bearer.pop();
            }
        }
        if bearer.is_empty() {
            return Err("--bearer-stdin must not be empty".to_string());
        }
        return Ok(Zeroizing::new(bearer));
    }
    match std::env::var(BEARER_ENV) {
        Ok(v) if !v.is_empty() => Ok(Zeroizing::new(v)),
        _ => Err(format!(
            "missing bearer: set `{BEARER_ENV}` (run `eval \"$(bw env --tenant <t>)\"`) or pass `--bearer-stdin`"
        )),
    }
}

#[derive(Deserialize, Debug)]
struct WrappedExportKeyResponse {
    /// URL-safe-base64 (no pad) wrapped export_key bytes. The
    /// broker wraps under its process-local wrapping key; we
    /// don't unwrap client-side — the wrapped form IS the value
    /// HKDF derives the v4 master key from. Restricting the
    /// HKDF to consume the wrapped form gives v4 the "broker
    /// restart invalidates every active lease" property
    /// transitively: the wrapping key rolls and the wrapped
    /// bytes change, and the HKDF output the vault uses
    /// changes too.
    wrapped_export_key: String,
    suite_version: u8,
}

/// Fetch the wrapped export_key + suite_version for the currently
/// supplied bearer.
// Not covered in the unit tier: builds a tokio runtime and performs the
// real HTTP GET to /auth/lease/wrapped-export-key. Covered by the
// docker-gated opaque_e2e tier (auth-broker/tests/opaque_e2e.rs).
#[cfg(not(tarpaulin_include))]
fn fetch_wrapped_export_key(
    server: &str,
    bearer: &str,
    ca_path: Option<&Path>,
) -> Result<(Zeroizing<Vec<u8>>, u8), String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    rt.block_on(async move {
        let url = format!(
            "{}/auth/lease/wrapped-export-key",
            server.trim_end_matches('/')
        );
        let client = build_http_client_with_ca(ca_path)?;
        let response = client
            .get(&url)
            .header("authorization", format!("Bearer {bearer}"))
            .send()
            .await
            .map_err(|e| format!("network error talking to {url}: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "server returned status {} from {url}: {body}",
                status.as_u16()
            ));
        }
        let parsed: WrappedExportKeyResponse = serde_json::from_str(&body)
            .map_err(|e| format!("malformed response from {url}: {e}; body: {body}"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(parsed.wrapped_export_key.as_bytes())
            .map_err(|e| format!("`wrapped_export_key` is not valid url-safe-base64: {e}"))?;
        Ok((Zeroizing::new(bytes), parsed.suite_version))
    })
}

fn build_http_client_with_ca(ca_path: Option<&Path>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(path) = ca_path {
        let pem = std::fs::read(path)
            .map_err(|e| format!("failed to read --cacert file {}: {e}", path.display()))?;
        let p = path.display();
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("failed to parse PEM certificate(s) from {p}: {e}"))?;
        if certs.is_empty() {
            return Err(format!(
                "no valid PEM certificate found in {}",
                path.display()
            ));
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))
}

fn default_init_root(name: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/botwork/vault/{name}"))
}

fn default_root_from_env(user: Option<&str>, logname: Option<&str>) -> PathBuf {
    let user = user
        .filter(|value| !value.is_empty())
        .or_else(|| logname.filter(|value| !value.is_empty()))
        .unwrap_or("default");
    PathBuf::from(format!("/var/lib/botwork/vault/{user}"))
}

fn default_root() -> PathBuf {
    let user = std::env::var("USER").ok();
    let logname = std::env::var("LOGNAME").ok();
    default_root_from_env(user.as_deref(), logname.as_deref())
}

#[derive(Parser)]
#[command(name = "botwork-vault", about = "Manage botwork secret vaults", version = VERSION)]
struct Cli {
    /// Auth-broker base URL. Resolution: `--server` > `BOTWORK_LOGIN_SERVER` > built-in.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Path to a PEM CA certificate (bundle) to trust in addition to the system roots; overrides $SSL_CERT_FILE.
    #[arg(long, global = true, value_name = "PATH")]
    cacert: Option<PathBuf>,
    /// Read bearer from stdin.
    #[arg(long, global = true)]
    bearer_stdin: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a fresh vault keyed off the currently-bearer-bound
    /// lease.
    ///
    /// Calls `GET /auth/lease/wrapped-export-key` on the broker, then
    /// HKDF-derives the master key from the returned bytes plus
    /// a freshly-generated per-vault salt. Refuses to clobber an
    /// existing file unless `--force` is set.
    Init {
        #[command(flatten)]
        init: InitArgs,
    },
    /// Quick load + unlock check for a vault file. Doesn't mutate.
    Verify {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Add {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        service: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long = "from-file")]
        from_file: Option<PathBuf>,
        #[arg(long = "value-stdin")]
        value_stdin: bool,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "allow-consumer")]
        allow_consumers: Vec<String>,
        /// Overwrite an existing secret rather than returning an error.
        #[arg(long)]
        overwrite: bool,
    },
    /// Alias for `add`.
    PutSecret {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        service: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long = "from-file")]
        from_file: Option<PathBuf>,
        #[arg(long = "value-stdin")]
        value_stdin: bool,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "allow-consumer")]
        allow_consumers: Vec<String>,
        /// Overwrite an existing secret rather than returning an error.
        #[arg(long)]
        overwrite: bool,
    },
    Get {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        service: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        raw: bool,
    },
    List {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Delete {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        service: String,
        #[arg(long)]
        name: String,
    },
    /// Manage plaintext public-key material (SSH). Never touches
    /// vault.botwork.
    Pubkey {
        #[command(subcommand)]
        action: PubkeyCommands,
    },
}

#[derive(clap::Args)]
struct InitArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long, default_value = "default")]
    name: String,
    #[arg(long)]
    force: bool,
    #[arg(long = "yes-really-overwrite")]
    yes_really_overwrite: bool,
}

#[derive(Subcommand)]
enum PubkeyCommands {
    /// Store a public key for a label.
    Add {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Key kind; only `ssh` is supported.
        #[arg(long)]
        kind: String,
        /// Label for this key (^[a-z0-9][a-z0-9._-]{0,62}$).
        #[arg(long)]
        label: String,
        /// File containing the OpenSSH public key line.
        #[arg(long = "from-file")]
        from_file: PathBuf,
        /// Overwrite an existing label without error.
        #[arg(long)]
        force: bool,
    },
    /// List stored public keys.
    List {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Key kind; only `ssh` is supported.
        #[arg(long)]
        kind: String,
        /// Emit JSON instead of TSV.
        #[arg(long)]
        json: bool,
    },
    /// Remove a stored public key.
    Delete {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Key kind; only `ssh` is supported.
        #[arg(long)]
        kind: String,
        /// Label to remove.
        #[arg(long)]
        label: String,
    },
    /// Concatenate all stored public keys to stdout
    /// (AuthorizedKeysCommand format).
    Cat {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Key kind; only `ssh` is supported.
        #[arg(long)]
        kind: String,
    },
}

#[derive(Serialize)]
struct SecretListItem {
    service: String,
    name: String,
    kind: String,
    created_at: i64,
    updated_at: i64,
    last_used_at: Option<i64>,
    tags: Vec<String>,
    allowed_consumers: Vec<String>,
}

#[derive(Serialize)]
struct SshListItem {
    label: String,
    #[serde(rename = "type")]
    key_type: String,
    comment: String,
}

fn require_ssh_kind(kind: &str) -> Result<(), CliFailure> {
    if kind != "ssh" {
        return Err(CliFailure::usage(format!("unsupported kind: {kind}")));
    }
    Ok(())
}

fn map_pubkey_error(err: VaultError) -> CliFailure {
    match err {
        VaultError::PublicStore(msg)
            if msg.starts_with("label already exists:") || msg.starts_with("no such label:") =>
        {
            CliFailure::conflict(msg)
        }
        VaultError::PublicStore(msg) => CliFailure::usage(msg),
        VaultError::UnsupportedVersion { .. } => CliFailure::unsupported_format(),
        other => CliFailure::usage(other.to_string()),
    }
}

fn map_vault_error(err: VaultError) -> CliFailure {
    match err {
        VaultError::UnsupportedVersion { .. } => CliFailure::unsupported_format(),
        VaultError::AlreadyInitialized(path) => CliFailure::conflict(format!(
            "vault root already exists and is non-empty: {}",
            path.display()
        )),
        other => CliFailure::usage(other.to_string()),
    }
}

fn validate_init_args(force: bool, yes_really_overwrite: bool) -> Result<(), CliFailure> {
    if force && !yes_really_overwrite {
        return Err(CliFailure::usage("--force requires --yes-really-overwrite"));
    }
    Ok(())
}

/// Read a value to seal under `--from-file` / `--value-stdin`.
#[allow(dead_code)]
fn read_value(from_file: Option<&PathBuf>, value_stdin: bool) -> Result<Vec<u8>, CliFailure> {
    let mut stdin = std::io::stdin().lock();
    read_value_from(from_file, value_stdin, &mut stdin)
}

fn read_value_from<R: std::io::Read>(
    from_file: Option<&PathBuf>,
    value_stdin: bool,
    reader: &mut R,
) -> Result<Vec<u8>, CliFailure> {
    if let Some(path) = from_file {
        return fs::read(path).map_err(|e| {
            CliFailure::usage(format!(
                "failed to read --from-file {}: {e}",
                path.display()
            ))
        });
    }
    if value_stdin {
        let mut buf = Vec::new();
        reader
            .read_to_end_via_buf(&mut buf)
            .map_err(|e| CliFailure::usage(format!("failed to read value from stdin: {e}")))?;
        return Ok(buf);
    }
    Err(CliFailure::usage(
        "must supply either --from-file <path> or --value-stdin",
    ))
}

/// Newtype helper so the call site stays readable; `Read::read_to_end`
/// isn't in `prelude::*` here.
trait ReadToEndViaBuf {
    fn read_to_end_via_buf(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize>;
}

impl<R: std::io::Read> ReadToEndViaBuf for R {
    fn read_to_end_via_buf(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        self.read_to_end(buf)
    }
}

fn sanitize_secret_value(
    mut value: Vec<u8>,
    kind: SecretKind,
    from_file_used: bool,
) -> Result<SanitizedValue, CliFailure> {
    let header_bound = matches!(
        kind,
        SecretKind::ApiKey
            | SecretKind::OauthToken
            | SecretKind::Password
            | SecretKind::SshPublicKey
    );
    if !header_bound {
        return Ok(SanitizedValue {
            value,
            warning: None,
        });
    }
    let stripped = if value.ends_with(b"\r\n") {
        value.truncate(value.len() - 2);
        true
    } else if value.ends_with(b"\n") {
        value.pop();
        true
    } else {
        false
    };
    if value.iter().any(|&b| b == b'\n' || b == b'\r' || b == 0x00) {
        return Err(CliFailure::usage(format!(
            "secret value (kind={kind}) contains an embedded control character \
             (\\n, \\r, or \\0) which is invalid in an HTTP header value"
        )));
    }
    Ok(SanitizedValue {
        value,
        warning: (stripped && from_file_used).then_some(
            "WARNING: trailing newline stripped from --from-file value \
             (this is usually what you want when storing a credential)"
                .to_string(),
        ),
    })
}

fn prepare_put_input(
    kind: String,
    from_file: Option<PathBuf>,
    value_stdin: bool,
) -> Result<PreparedPutInput, CliFailure> {
    let mut stdin = std::io::stdin().lock();
    prepare_put_input_from(kind, from_file, value_stdin, &mut stdin)
}

fn prepare_put_input_from<R: std::io::Read>(
    kind: String,
    from_file: Option<PathBuf>,
    value_stdin: bool,
    reader: &mut R,
) -> Result<PreparedPutInput, CliFailure> {
    let kind: SecretKind = kind.parse().map_err(CliFailure::usage)?;
    let from_file_used = from_file.is_some();
    let value = read_value_from(from_file.as_ref(), value_stdin, reader)?;
    let sanitized = sanitize_secret_value(value, kind, from_file_used)?;
    Ok(PreparedPutInput { kind, sanitized })
}

// Not covered in the unit tier: thin wrapper over fetch_wrapped_export_key,
// which requires a live auth-broker HTTP endpoint. Covered by the
// docker-gated opaque_e2e tier (auth-broker/tests/opaque_e2e.rs).
#[cfg(not(tarpaulin_include))]
fn fetch_cli_key_material(
    server: &str,
    bearer: &str,
    ca_path: Option<&Path>,
) -> Result<(Zeroizing<Vec<u8>>, u8), CliFailure> {
    fetch_wrapped_export_key(server, bearer, ca_path).map_err(CliFailure::usage)
}

fn run_init(
    args: InitArgs,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = args.root.unwrap_or_else(|| default_init_root(&args.name));
    validate_init_args(args.force, args.yes_really_overwrite)?;

    if args.force && args.yes_really_overwrite && root.exists() {
        fs::remove_dir_all(&root).map_err(|e| CliFailure::usage(e.to_string()))?;
    }

    Vault::create(&root, wrapped_export_key, suite).map_err(map_vault_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!("{}\n", root.display()))),
        stderr: vec![
            format!(
                "✓ Wrapped export_key fetched from auth-broker (suite_version={suite}, {} bytes)",
                wrapped_export_key.len()
            ),
            format!("✓ vault initialised at {}", root.display()),
        ],
    })
}

fn run_verify(
    root: Option<PathBuf>,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = root.unwrap_or_else(default_root);
    let mut vault = Vault::new(&root);
    vault
        .unlock(wrapped_export_key, suite)
        .map_err(map_vault_error)?;
    let n = vault.list_secrets().map(|v| v.len()).unwrap_or(0);
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!(
            "ok: {} ({n} entries)\n",
            root.display()
        ))),
        stderr: vec![],
    })
}

#[allow(clippy::too_many_arguments)]
fn run_put(
    root: Option<PathBuf>,
    service: String,
    name: String,
    prepared: PreparedPutInput,
    tags: Vec<String>,
    allow_consumers: Vec<String>,
    overwrite: bool,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = root.unwrap_or_else(default_root);

    let mut vault = Vault::new(&root);
    vault
        .unlock(wrapped_export_key, suite)
        .map_err(map_vault_error)?;

    // Overwrite gate: without --overwrite, fail if the secret already exists.
    let key = SecretKey {
        service: service.clone(),
        name: name.clone(),
    };
    if !overwrite {
        match vault.has_secret(&key) {
            Ok(true) => {
                return Err(CliFailure::conflict(format!(
                    "secret '{service}/{name}' already exists; \
                     use --overwrite to replace it"
                )));
            }
            Ok(false) => {}
            Err(e) => return Err(map_vault_error(e)),
        }
    }

    let now = chrono::Utc::now().timestamp();
    let entry = SecretEntry {
        kind: prepared.kind,
        value: prepared.sanitized.value,
        created_at: now,
        updated_at: now,
        last_used_at: None,
        tags,
        allowed_consumers: allow_consumers,
    };
    vault.put_secret(key, entry).map_err(map_vault_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!("stored {service}/{name}\n"))),
        stderr: prepared.sanitized.warning.into_iter().collect(),
    })
}

fn run_get(
    root: Option<PathBuf>,
    service: String,
    name: String,
    raw: bool,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = root.unwrap_or_else(default_root);
    let mut vault = Vault::new(&root);
    vault
        .unlock(wrapped_export_key, suite)
        .map_err(map_vault_error)?;
    let key = SecretKey { service, name };
    let entry = vault.get_secret(&key).map_err(map_vault_error)?;
    let stdout = if raw {
        StdoutPayload::Bytes(entry.value.to_vec())
    } else {
        let text = match std::str::from_utf8(&entry.value) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&entry.value).into_owned(),
        };
        StdoutPayload::Text(format!("{text}\n"))
    };
    Ok(CommandOutcome {
        stdout: Some(stdout),
        stderr: vec![],
    })
}

fn run_list(
    root: Option<PathBuf>,
    json: bool,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = root.unwrap_or_else(default_root);
    let mut vault = Vault::new(&root);
    vault
        .unlock(wrapped_export_key, suite)
        .map_err(map_vault_error)?;
    let secrets = vault.list_secrets().map_err(map_vault_error)?;

    let stdout = if json {
        let items: Vec<SecretListItem> = secrets
            .into_iter()
            .map(|(k, m)| SecretListItem {
                service: k.service,
                name: k.name,
                kind: m.kind.to_string(),
                created_at: m.created_at,
                updated_at: m.updated_at,
                last_used_at: m.last_used_at,
                tags: m.tags,
                allowed_consumers: m.allowed_consumers,
            })
            .collect();
        StdoutPayload::Text(format!(
            "{}\n",
            serde_json::to_string(&items).map_err(|e| CliFailure::usage(e.to_string()))?
        ))
    } else {
        let body = secrets
            .into_iter()
            .map(|(k, m)| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    k.service,
                    k.name,
                    m.kind,
                    m.created_at,
                    m.last_used_at
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    m.tags.join(","),
                    m.allowed_consumers.join(",")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if body.is_empty() {
            return Ok(CommandOutcome::default());
        }
        StdoutPayload::Text(format!("{body}\n"))
    };

    Ok(CommandOutcome {
        stdout: Some(stdout),
        stderr: vec![],
    })
}

fn run_delete(
    root: Option<PathBuf>,
    service: String,
    name: String,
    wrapped_export_key: &[u8],
    suite: u8,
) -> Result<CommandOutcome, CliFailure> {
    let root = root.unwrap_or_else(default_root);
    let mut vault = Vault::new(&root);
    vault
        .unlock(wrapped_export_key, suite)
        .map_err(map_vault_error)?;
    let key = SecretKey {
        service: service.clone(),
        name: name.clone(),
    };
    vault.delete_secret(&key).map_err(map_vault_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!("deleted {service}/{name}\n"))),
        stderr: vec![],
    })
}

fn run_pubkey_add(
    root: Option<PathBuf>,
    kind: String,
    label: String,
    from_file: PathBuf,
    force: bool,
) -> Result<CommandOutcome, CliFailure> {
    require_ssh_kind(&kind)?;
    let root = root.unwrap_or_else(default_root);
    let key_line = fs::read_to_string(&from_file).map_err(|e| {
        let msg = format!("failed to read --from-file {}: {e}", from_file.display());
        CliFailure::usage(msg)
    })?;
    let store = PublicStore::new(&root);
    store
        .add_ssh(&label, &key_line, force)
        .map_err(map_pubkey_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!("stored {label}\n"))),
        stderr: vec![],
    })
}

fn run_pubkey_list(
    root: Option<PathBuf>,
    kind: String,
    json: bool,
) -> Result<CommandOutcome, CliFailure> {
    require_ssh_kind(&kind)?;
    let root = root.unwrap_or_else(default_root);
    let store = PublicStore::new(&root);
    let entries = store.list_ssh().map_err(map_pubkey_error)?;
    let stdout = if json {
        let items: Vec<SshListItem> = entries
            .into_iter()
            .map(|e| SshListItem {
                label: e.label,
                key_type: e.key_type,
                comment: e.comment,
            })
            .collect();
        StdoutPayload::Text(format!(
            "{}\n",
            serde_json::to_string(&items).map_err(|e| CliFailure::usage(e.to_string()))?
        ))
    } else {
        let body = entries
            .into_iter()
            .map(|e| format!("{}\t{}\t{}", e.label, e.key_type, e.comment))
            .collect::<Vec<_>>()
            .join("\n");
        if body.is_empty() {
            return Ok(CommandOutcome::default());
        }
        StdoutPayload::Text(format!("{body}\n"))
    };
    Ok(CommandOutcome {
        stdout: Some(stdout),
        stderr: vec![],
    })
}

fn run_pubkey_delete(
    root: Option<PathBuf>,
    kind: String,
    label: String,
) -> Result<CommandOutcome, CliFailure> {
    require_ssh_kind(&kind)?;
    let root = root.unwrap_or_else(default_root);
    let store = PublicStore::new(&root);
    store.delete_ssh(&label).map_err(map_pubkey_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(format!("deleted {label}\n"))),
        stderr: vec![],
    })
}

fn run_pubkey_cat(root: Option<PathBuf>, kind: String) -> Result<CommandOutcome, CliFailure> {
    require_ssh_kind(&kind)?;
    let root = root.unwrap_or_else(default_root);
    let store = PublicStore::new(&root);
    let out = store.cat_ssh().map_err(map_pubkey_error)?;
    Ok(CommandOutcome {
        stdout: Some(StdoutPayload::Text(out)),
        stderr: vec![],
    })
}

// Not covered in the unit tier: every arm's residual uncovered lines are
// the fetch_cli_key_material(...) call, which requires a live auth-broker
// HTTP endpoint. Covered by the docker-gated opaque_e2e tier.
#[cfg(not(tarpaulin_include))]
fn run(cli: Cli) -> Result<CommandOutcome, CliFailure> {
    let server = resolve_server(cli.server.as_deref());
    let cacert = resolve_cacert(cli.cacert.as_deref());

    match cli.command {
        Commands::Init { init } => {
            validate_init_args(init.force, init.yes_really_overwrite)?;
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_init(init, &wrapped, suite)
        }
        Commands::Verify { root } => {
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_verify(root, &wrapped, suite)
        }
        Commands::Add {
            root,
            service,
            name,
            kind,
            from_file,
            value_stdin,
            tags,
            allow_consumers,
            overwrite,
        }
        | Commands::PutSecret {
            root,
            service,
            name,
            kind,
            from_file,
            value_stdin,
            tags,
            allow_consumers,
            overwrite,
        } => {
            let prepared = prepare_put_input(kind, from_file, value_stdin)?;
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_put(
                root,
                service,
                name,
                prepared,
                tags,
                allow_consumers,
                overwrite,
                &wrapped,
                suite,
            )
        }
        Commands::Get {
            root,
            service,
            name,
            raw,
        } => {
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_get(root, service, name, raw, &wrapped, suite)
        }
        Commands::List { root, json } => {
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_list(root, json, &wrapped, suite)
        }
        Commands::Delete {
            root,
            service,
            name,
        } => {
            let bearer = resolve_bearer(cli.bearer_stdin)?;
            let (wrapped, suite) = fetch_cli_key_material(&server, &bearer, cacert.as_deref())?;
            run_delete(root, service, name, &wrapped, suite)
        }
        Commands::Pubkey { action } => match action {
            PubkeyCommands::Add {
                root,
                kind,
                label,
                from_file,
                force,
            } => run_pubkey_add(root, kind, label, from_file, force),
            PubkeyCommands::List { root, kind, json } => run_pubkey_list(root, kind, json),
            PubkeyCommands::Delete { root, kind, label } => run_pubkey_delete(root, kind, label),
            PubkeyCommands::Cat { root, kind } => run_pubkey_cat(root, kind),
        },
    }
}

// Not covered in the unit tier: process entrypoint — tracing init,
// Cli::parse, and process::exit mapping. Covered end-to-end by the
// docker-gated opaque_e2e tier.
#[cfg(not(tarpaulin_include))]
fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    info!(
        "{PREFIX} botwork-vault {}",
        botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
    );

    match run(Cli::parse()) {
        Ok(outcome) => {
            if let Err(err) = emit_outcome(outcome) {
                emit_failure(err);
            }
        }
        Err(err) => emit_failure(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use std::io::Cursor;
    use std::sync::Mutex;
    use tempfile::TempDir;

    const FAST_SUITE: u8 = 1;
    const TEST_EXPORT_KEY: &[u8; 64] =
        b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    const SINGLE_CERT_PEM: &[u8] = b"
        -----BEGIN CERTIFICATE-----
        MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
        ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
        QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
        ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
        BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
        YyRIHN8wfdVoOw==
        -----END CERTIFICATE-----
    ";

    const PEM_BUNDLE: &[u8] = b"
        -----BEGIN CERTIFICATE-----
        MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
        ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
        QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
        ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
        BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
        YyRIHN8wfdVoOw==
        -----END CERTIFICATE-----
        -----BEGIN CERTIFICATE-----
        MIIB8jCCAXigAwIBAgITBmyf18G7EEwpQ+Vxe3ssyBrBDjAKBggqhkjOPQQDAzA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSA0MB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgNDB2MBAGByqGSM49AgEGBSuBBAAiA2IABNKrijdPo1MN/sGKe0uoe0ZLY7Bi
        9i0b2whxIdIA6GO9mif78DluXeo9pcmBqqNbIJhFXRbb/egQbeOc4OO9X4Ri83Bk
        M6DLJC9wuoihKqB1+IGuYgbEgds5bimwHvouXKNCMEAwDwYDVR0TAQH/BAUwAwEB
        /zAOBgNVHQ8BAf8EBAMCAYYwHQYDVR0OBBYEFNPsxzplbszh2naaVvuc84ZtV+WB
        MAoGCCqGSM49BAMDA2gAMGUCMDqLIfG9fhGt0O9Yli/W651+kI0rz2ZVwyzjKKlw
        CkcO8DdZEv8tmZQoTipPNU0zWgIxAOp1AE47xDqUEpHJWEadIRNyp4iciuRMStuW
        1KyLa2tJElMzrdfkviT8tQp21KW8EA==
        -----END CERTIFICATE-----
    ";

    fn ssl_cert_file_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    fn bearer_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    fn write_temp_pem(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        use std::io::Write as _;
        let normalized = String::from_utf8_lossy(contents)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        file.write_all(normalized.as_bytes()).expect("write pem");
        file.write_all(b"\n").expect("final newline");
        file
    }

    fn lock_env(mutex: &'static Mutex<()>) -> std::sync::MutexGuard<'static, ()> {
        mutex.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn make_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        (dir, root)
    }

    fn init_vault(root: &Path) {
        Vault::create(root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    }

    fn sample_key(name: &str) -> SecretKey {
        SecretKey {
            service: "svc".to_string(),
            name: name.to_string(),
        }
    }

    fn sample_entry(value: &[u8]) -> SecretEntry {
        let now = chrono::Utc::now().timestamp();
        SecretEntry {
            kind: SecretKind::ApiKey,
            value: value.to_vec(),
            created_at: now,
            updated_at: now,
            last_used_at: None,
            tags: vec!["env:test".to_string()],
            allowed_consumers: vec!["plugin".to_string()],
        }
    }

    #[test]
    fn version_flag_long_reports_display_version() {
        let err = Cli::try_parse_from(["botwork-vault", "--version"])
            .err()
            .expect("expected DisplayVersion error");
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let msg = err.to_string();
        assert!(
            msg.contains(VERSION),
            "expected version string {VERSION}, got: {msg:?}",
        );
    }

    #[test]
    fn version_flag_short_reports_display_version() {
        let err = Cli::try_parse_from(["botwork-vault", "-V"])
            .err()
            .expect("expected DisplayVersion error");
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn resolve_cacert_prefers_cli_over_env() {
        let _lock = lock_env(ssl_cert_file_env_lock());
        std::env::set_var(SSL_CERT_FILE_ENV, "/tmp/from-env.pem");
        let resolved = resolve_cacert(Some(Path::new("/tmp/from-cli.pem")));
        assert_eq!(resolved, Some(PathBuf::from("/tmp/from-cli.pem")));
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn resolve_cacert_uses_env_when_cli_missing() {
        let _lock = lock_env(ssl_cert_file_env_lock());
        std::env::set_var(SSL_CERT_FILE_ENV, "/tmp/from-env.pem");
        let resolved = resolve_cacert(None);
        assert_eq!(resolved, Some(PathBuf::from("/tmp/from-env.pem")));
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn resolve_cacert_ignores_empty_env() {
        let _lock = lock_env(ssl_cert_file_env_lock());
        std::env::set_var(SSL_CERT_FILE_ENV, " ");
        let resolved = resolve_cacert(None);
        assert_eq!(resolved, None);
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn build_http_client_with_single_cert_pem_ok() {
        let file = write_temp_pem(SINGLE_CERT_PEM);
        let client = build_http_client_with_ca(Some(file.path()));
        assert!(client.is_ok(), "expected success, got: {client:?}");
    }

    #[test]
    fn build_http_client_with_pem_bundle_ok() {
        let file = write_temp_pem(PEM_BUNDLE);
        let client = build_http_client_with_ca(Some(file.path()));
        assert!(client.is_ok(), "expected success, got: {client:?}");
    }

    #[test]
    fn build_http_client_with_invalid_pem_is_actionable() {
        let file = write_temp_pem(b"not pem");
        let err = build_http_client_with_ca(Some(file.path())).unwrap_err();
        assert!(
            err.contains("failed to parse PEM certificate(s) from")
                || err.contains("no valid PEM certificate found in")
        );
        assert!(err.contains(&file.path().display().to_string()));
    }

    #[test]
    fn resolve_bearer_uses_stdin_before_env() {
        let _lock = lock_env(bearer_env_lock());
        std::env::set_var(BEARER_ENV, "from-env");
        let mut stdin = Cursor::new(b"from-stdin\n".to_vec());
        let bearer = resolve_bearer_from_sources(true, &mut stdin).unwrap();
        assert_eq!(bearer.as_str(), "from-stdin");
        std::env::remove_var(BEARER_ENV);
    }

    #[test]
    fn resolve_bearer_stdin_rejects_empty_after_newline_trim() {
        let _lock = lock_env(bearer_env_lock());
        std::env::remove_var(BEARER_ENV);
        let mut stdin = Cursor::new(b"\n".to_vec());
        let err = resolve_bearer_from_sources(true, &mut stdin).unwrap_err();
        assert_eq!(err, "--bearer-stdin must not be empty");
    }

    #[test]
    fn resolve_bearer_stdin_trims_crlf() {
        let _lock = lock_env(bearer_env_lock());
        std::env::remove_var(BEARER_ENV);
        let mut stdin = Cursor::new(b"tok\r\n".to_vec());
        let bearer = resolve_bearer_from_sources(true, &mut stdin).unwrap();
        assert_eq!(bearer.as_str(), "tok");
    }

    #[test]
    fn resolve_bearer_missing_message_mentions_all_sources() {
        let _lock = lock_env(bearer_env_lock());
        std::env::remove_var(BEARER_ENV);
        let mut stdin = Cursor::new(Vec::<u8>::new());
        let err = resolve_bearer_from_sources(false, &mut stdin).unwrap_err();
        assert!(err.contains(BEARER_ENV));
        assert!(err.contains("--bearer-stdin"));
    }

    #[test]
    fn default_root_resolution_prefers_user_then_logname_then_default() {
        assert_eq!(
            default_root_from_env(Some("alice"), Some("bob")),
            PathBuf::from("/var/lib/botwork/vault/alice")
        );
        assert_eq!(
            default_root_from_env(None, Some("bob")),
            PathBuf::from("/var/lib/botwork/vault/bob")
        );
        assert_eq!(
            default_root_from_env(Some(""), Some("")),
            PathBuf::from("/var/lib/botwork/vault/default")
        );
    }

    #[test]
    fn validate_init_args_preserves_force_confirmation_rule() {
        let err = validate_init_args(true, false).unwrap_err();
        assert_eq!(err.exit_code, ExitCode::Usage);
        assert_eq!(err.message, "--force requires --yes-really-overwrite");
        validate_init_args(false, false).unwrap();
        validate_init_args(true, true).unwrap();
    }

    #[test]
    fn sanitize_secret_value_strips_trailing_newline_for_header_bound_kind() {
        let sanitized =
            sanitize_secret_value(b"ghp_abc123\r\n".to_vec(), SecretKind::ApiKey, true).unwrap();
        assert_eq!(sanitized.value, b"ghp_abc123");
        assert_eq!(
            sanitized.warning.as_deref(),
            Some(
                "WARNING: trailing newline stripped from --from-file value \
                 (this is usually what you want when storing a credential)"
            )
        );
    }

    #[test]
    fn sanitize_secret_value_strips_trailing_lf_for_header_bound_kind() {
        let sanitized =
            sanitize_secret_value(b"ghp_tok\n".to_vec(), SecretKind::ApiKey, true).unwrap();
        assert_eq!(sanitized.value, b"ghp_tok");
        assert_eq!(
            sanitized.warning.as_deref(),
            Some(
                "WARNING: trailing newline stripped from --from-file value \
                 (this is usually what you want when storing a credential)"
            )
        );
    }

    #[test]
    fn sanitize_secret_value_rejects_embedded_control_characters() {
        let err =
            sanitize_secret_value(b"ghp_abc\n123".to_vec(), SecretKind::ApiKey, true).unwrap_err();
        assert_eq!(err.exit_code, ExitCode::Usage);
        assert!(err.message.contains("embedded control character"));
    }

    #[test]
    fn sanitize_secret_value_leaves_non_header_bound_values_alone() {
        let sanitized =
            sanitize_secret_value(b"pem-data\n".to_vec(), SecretKind::Pem, true).unwrap();
        assert_eq!(sanitized.value, b"pem-data\n");
        assert_eq!(sanitized.warning, None);
    }

    #[test]
    fn map_vault_error_and_pubkey_error_preserve_exit_codes() {
        let unsupported = map_vault_error(VaultError::UnsupportedVersion {
            path: PathBuf::from("/tmp/vault.botwork"),
        });
        assert_eq!(unsupported.exit_code, ExitCode::UnsupportedFormat);
        assert_eq!(unsupported.message, "unsupported vault format");

        let conflict = map_pubkey_error(VaultError::PublicStore(
            "label already exists: mykey".to_string(),
        ));
        assert_eq!(conflict.exit_code, ExitCode::Conflict);
        assert_eq!(conflict.message, "label already exists: mykey");

        let pubkey_unsupported = map_pubkey_error(VaultError::UnsupportedVersion {
            path: PathBuf::from("/tmp/public.botwork"),
        });
        assert_eq!(pubkey_unsupported.exit_code, ExitCode::UnsupportedFormat);
        assert_eq!(pubkey_unsupported.message, "unsupported vault format");

        let pubkey_usage = map_pubkey_error(VaultError::PublicStore("plain message".to_string()));
        assert_eq!(pubkey_usage.exit_code, ExitCode::Usage);
        assert_eq!(pubkey_usage.message, "plain message");

        let pubkey_fallback = map_pubkey_error(VaultError::Locked);
        assert_eq!(pubkey_fallback.exit_code, ExitCode::Usage);
        assert_eq!(pubkey_fallback.message, "vault is locked");

        let already_initialized = map_vault_error(VaultError::AlreadyInitialized(PathBuf::from(
            "/tmp/existing-vault",
        )));
        assert_eq!(already_initialized.exit_code, ExitCode::Conflict);
        assert!(already_initialized
            .message
            .contains("already exists and is non-empty"));
        assert!(already_initialized.message.contains("/tmp/existing-vault"));

        let vault_fallback = map_vault_error(VaultError::Locked);
        assert_eq!(vault_fallback.exit_code, ExitCode::Usage);
        assert_eq!(vault_fallback.message, "vault is locked");
    }

    #[test]
    fn run_init_creates_vault_and_reports_success_messages() {
        let (_dir, root) = make_root();
        let outcome = run_init(
            InitArgs {
                root: Some(root.clone()),
                name: "ignored".to_string(),
                force: false,
                yes_really_overwrite: false,
            },
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(
            outcome.stderr,
            vec![
                format!(
                    "✓ Wrapped export_key fetched from auth-broker (suite_version={FAST_SUITE}, {} bytes)",
                    TEST_EXPORT_KEY.len()
                ),
                format!("✓ vault initialised at {}", root.display()),
            ]
        );
        assert_eq!(
            outcome.stdout,
            Some(StdoutPayload::Text(format!("{}\n", root.display())))
        );
    }

    #[test]
    fn run_init_force_overwrite_removes_existing_root() {
        let (_dir, root) = make_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("stale"), b"stale").unwrap();
        let outcome = run_init(
            InitArgs {
                root: Some(root.clone()),
                name: "ignored".to_string(),
                force: true,
                yes_really_overwrite: true,
            },
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(
            outcome.stdout,
            Some(StdoutPayload::Text(format!("{}\n", root.display())))
        );
        assert!(!root.join("stale").exists());
    }

    #[test]
    fn run_verify_list_get_and_delete_round_trip_without_shell_exits() {
        let (_dir, root) = make_root();
        let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        vault
            .put_secret(sample_key("token"), sample_entry(b"value"))
            .unwrap();
        vault.lock();

        let verify = run_verify(Some(root.clone()), TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        assert_eq!(
            verify.stdout,
            Some(StdoutPayload::Text(format!(
                "ok: {} (1 entries)\n",
                root.display()
            )))
        );

        let list = run_list(Some(root.clone()), false, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        match list.stdout.unwrap() {
            StdoutPayload::Text(body) => {
                assert!(body.contains("svc\ttoken\tapi-key"));
                assert!(body.ends_with('\n'));
            }
            other => panic!("unexpected stdout payload: {other:?}"),
        }

        let get = run_get(
            Some(root.clone()),
            "svc".to_string(),
            "token".to_string(),
            false,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(get.stdout, Some(StdoutPayload::Text("value\n".to_string())));

        let delete = run_delete(
            Some(root.clone()),
            "svc".to_string(),
            "token".to_string(),
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(
            delete.stdout,
            Some(StdoutPayload::Text("deleted svc/token\n".to_string()))
        );
    }

    #[test]
    fn run_get_raw_returns_bytes_payload() {
        let (_dir, root) = make_root();
        let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        vault
            .put_secret(sample_key("token"), sample_entry(b"value"))
            .unwrap();
        vault.lock();

        let get = run_get(
            Some(root),
            "svc".to_string(),
            "token".to_string(),
            true,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(get.stdout, Some(StdoutPayload::Bytes(b"value".to_vec())));
    }

    #[test]
    fn run_get_non_utf8_value_uses_lossy_fallback() {
        let (_dir, root) = make_root();
        let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        vault
            .put_secret(sample_key("binary"), sample_entry(&[0xff, 0xfe, b'a']))
            .unwrap();
        vault.lock();

        let get = run_get(
            Some(root),
            "svc".to_string(),
            "binary".to_string(),
            false,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        match get.stdout {
            Some(StdoutPayload::Text(text)) => {
                assert!(text.contains('\u{fffd}'));
                assert!(text.ends_with('\n'));
            }
            other => panic!("unexpected stdout payload: {other:?}"),
        }
    }

    #[test]
    fn run_pubkey_helpers_preserve_outputs() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        init_vault(&root);
        let key_path = dir.path().join("key.pub");
        std::fs::write(
            &key_path,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test-comment\n",
        )
        .unwrap();

        let add = run_pubkey_add(
            Some(root.clone()),
            "ssh".to_string(),
            "mykey".to_string(),
            key_path.clone(),
            false,
        )
        .unwrap();
        assert_eq!(
            add.stdout,
            Some(StdoutPayload::Text("stored mykey\n".to_string()))
        );

        let list = run_pubkey_list(Some(root.clone()), "ssh".to_string(), true).unwrap();
        match list.stdout.unwrap() {
            StdoutPayload::Text(body) => assert!(body.contains("\"label\":\"mykey\"")),
            other => panic!("unexpected stdout payload: {other:?}"),
        }

        let cat = run_pubkey_cat(Some(root.clone()), "ssh".to_string()).unwrap();
        assert_eq!(
            cat.stdout,
            Some(StdoutPayload::Text(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test-comment\n".to_string()
            ))
        );

        let delete = run_pubkey_delete(Some(root), "ssh".to_string(), "mykey".to_string()).unwrap();
        assert_eq!(
            delete.stdout,
            Some(StdoutPayload::Text("deleted mykey\n".to_string()))
        );
    }

    // --- emit_outcome ---

    #[test]
    fn emit_outcome_text_payload_returns_ok() {
        let outcome = CommandOutcome {
            stdout: Some(StdoutPayload::Text("hello\n".to_string())),
            stderr: vec!["warn: something".to_string()],
        };
        assert!(emit_outcome(outcome).is_ok());
    }

    #[test]
    fn emit_outcome_bytes_payload_returns_ok() {
        let outcome = CommandOutcome {
            stdout: Some(StdoutPayload::Bytes(b"raw bytes".to_vec())),
            stderr: vec![],
        };
        assert!(emit_outcome(outcome).is_ok());
    }

    #[test]
    fn emit_outcome_no_stdout_returns_ok() {
        let outcome = CommandOutcome {
            stdout: None,
            stderr: vec!["line1".to_string(), "line2".to_string()],
        };
        assert!(emit_outcome(outcome).is_ok());
    }

    // --- resolve_server ---

    fn server_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    #[test]
    fn resolve_server_prefers_cli_flag_over_env() {
        let _lock = lock_env(server_env_lock());
        std::env::set_var(DEFAULT_SERVER_ENV, "http://from-env:8080");
        let result = resolve_server(Some("http://from-cli:9999"));
        assert_eq!(result, "http://from-cli:9999");
        std::env::remove_var(DEFAULT_SERVER_ENV);
    }

    #[test]
    fn resolve_server_uses_env_when_cli_absent() {
        let _lock = lock_env(server_env_lock());
        std::env::set_var(DEFAULT_SERVER_ENV, "http://from-env:8080");
        let result = resolve_server(None);
        assert_eq!(result, "http://from-env:8080");
        std::env::remove_var(DEFAULT_SERVER_ENV);
    }

    #[test]
    fn resolve_server_falls_back_to_default_when_env_empty() {
        let _lock = lock_env(server_env_lock());
        std::env::set_var(DEFAULT_SERVER_ENV, "");
        let result = resolve_server(None);
        assert_eq!(result, DEFAULT_SERVER);
        std::env::remove_var(DEFAULT_SERVER_ENV);
    }

    #[test]
    fn resolve_server_falls_back_to_default_when_env_absent() {
        let _lock = lock_env(server_env_lock());
        std::env::remove_var(DEFAULT_SERVER_ENV);
        let result = resolve_server(None);
        assert_eq!(result, DEFAULT_SERVER);
    }

    // --- default_init_root / default_root ---

    fn user_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    #[test]
    fn default_init_root_returns_correct_path() {
        assert_eq!(
            default_init_root("foo"),
            std::path::PathBuf::from("/var/lib/botwork/vault/foo")
        );
        assert_eq!(
            default_init_root("bar"),
            std::path::PathBuf::from("/var/lib/botwork/vault/bar")
        );
    }

    #[test]
    fn default_root_delegates_to_env_helpers() {
        let _lock = lock_env(user_env_lock());
        std::env::set_var("USER", "alice");
        std::env::remove_var("LOGNAME");
        assert_eq!(
            default_root(),
            std::path::PathBuf::from("/var/lib/botwork/vault/alice")
        );

        std::env::remove_var("USER");
        std::env::set_var("LOGNAME", "bob");
        assert_eq!(
            default_root(),
            std::path::PathBuf::from("/var/lib/botwork/vault/bob")
        );

        std::env::remove_var("USER");
        std::env::remove_var("LOGNAME");
        assert_eq!(
            default_root(),
            std::path::PathBuf::from("/var/lib/botwork/vault/default")
        );
    }

    // --- run_put: happy path + overwrite conflict gate ---

    #[test]
    fn run_put_stores_secret_and_reports_success() {
        let (_dir, root) = make_root();
        init_vault(&root);

        let prepared = PreparedPutInput {
            kind: SecretKind::ApiKey,
            sanitized: SanitizedValue {
                value: b"s3cr3t".to_vec(),
                warning: None,
            },
        };
        let outcome = run_put(
            Some(root.clone()),
            "myservice".to_string(),
            "mykey".to_string(),
            prepared,
            vec![],
            vec![],
            false,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
        assert_eq!(
            outcome.stdout,
            Some(StdoutPayload::Text("stored myservice/mykey\n".to_string()))
        );
        assert!(outcome.stderr.is_empty());
    }

    #[test]
    fn run_put_overwrite_conflict_gate() {
        let (_dir, root) = make_root();
        init_vault(&root);

        let mk_prepared = || PreparedPutInput {
            kind: SecretKind::ApiKey,
            sanitized: SanitizedValue {
                value: b"v".to_vec(),
                warning: None,
            },
        };

        // First insert succeeds.
        run_put(
            Some(root.clone()),
            "svc".to_string(),
            "key".to_string(),
            mk_prepared(),
            vec![],
            vec![],
            false,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();

        // Second insert with overwrite=false returns Conflict.
        let err = run_put(
            Some(root.clone()),
            "svc".to_string(),
            "key".to_string(),
            mk_prepared(),
            vec![],
            vec![],
            false,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap_err();
        assert_eq!(err.exit_code, ExitCode::Conflict);
        assert!(
            err.message.contains("--overwrite"),
            "conflict message must mention --overwrite, got: {:?}",
            err.message
        );

        // Third insert with overwrite=true succeeds.
        run_put(
            Some(root.clone()),
            "svc".to_string(),
            "key".to_string(),
            mk_prepared(),
            vec![],
            vec![],
            true,
            TEST_EXPORT_KEY,
            FAST_SUITE,
        )
        .unwrap();
    }

    #[test]
    fn read_value_from_stdin_and_prepare_put_input_paths_work() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"file-secret").unwrap();
        let value = read_value(Some(&file.path().to_path_buf()), false).unwrap();
        assert_eq!(value, b"file-secret");

        let mut stdin = Cursor::new(b"secret-bytes".to_vec());
        let value = read_value_from(None, true, &mut stdin).unwrap();
        assert_eq!(value, b"secret-bytes");

        let mut stdin = Cursor::new(b"secret-bytes".to_vec());
        let prepared =
            prepare_put_input_from("api-key".to_string(), None, true, &mut stdin).unwrap();
        assert_eq!(prepared.kind, SecretKind::ApiKey);
        assert_eq!(prepared.sanitized.value, b"secret-bytes");
        assert_eq!(prepared.sanitized.warning, None);
    }

    #[test]
    fn read_value_from_and_prepare_put_input_require_a_value_source() {
        let mut stdin = Cursor::new(Vec::<u8>::new());
        let err = read_value_from(None, false, &mut stdin).unwrap_err();
        assert_eq!(
            err.message,
            "must supply either --from-file <path> or --value-stdin"
        );

        let err = prepare_put_input("api-key".to_string(), None, false)
            .err()
            .expect("missing input source should be rejected");
        assert_eq!(err.exit_code, ExitCode::Usage);
        assert_eq!(
            err.message,
            "must supply either --from-file <path> or --value-stdin"
        );
    }

    // --- run_list: JSON arm ---

    #[test]
    fn run_list_json_arm_emits_valid_json_with_expected_fields() {
        let (_dir, root) = make_root();
        let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        vault
            .put_secret(sample_key("token"), sample_entry(b"irrelevant"))
            .unwrap();
        vault.lock();

        let outcome = run_list(Some(root.clone()), true, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        let body = match outcome.stdout.unwrap() {
            StdoutPayload::Text(s) => s,
            other => panic!("expected Text payload, got: {other:?}"),
        };

        // Must be parseable JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(body.trim()).expect("run_list --json must emit valid JSON");

        let arr = parsed.as_array().expect("top-level JSON must be an array");
        assert!(!arr.is_empty(), "JSON array must contain the stored entry");
        let item = &arr[0];
        assert_eq!(item["service"], "svc");
        assert_eq!(item["name"], "token");
        assert_eq!(item["kind"], "api-key");
    }

    #[test]
    fn run_list_returns_default_outcome_for_empty_vault() {
        let (_dir, root) = make_root();
        init_vault(&root);

        let outcome = run_list(Some(root), false, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        assert_eq!(outcome, CommandOutcome::default());
    }

    #[test]
    fn run_pubkey_list_returns_default_outcome_for_empty_store() {
        let (_dir, root) = make_root();
        init_vault(&root);

        let outcome = run_pubkey_list(Some(root), "ssh".to_string(), false).unwrap();
        assert_eq!(outcome, CommandOutcome::default());
    }
}
