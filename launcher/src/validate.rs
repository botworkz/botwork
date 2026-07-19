use std::path::{Component, Path, PathBuf};

use regex::Regex;

use crate::error::LauncherError;

const STAGING_BASE: &str = "/var/lib/botwork/tenants";
const AGENTS_BASE: &str = "/var/lib/botwork/tenants";
const TENANT_RE: &str = r"[a-z][a-z0-9-]{0,30}";
pub const RESERVED_ENV_NAMES: &[&str] = &["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

/// Docker label namespace prefix the launcher / session-broker pair owns.
///
/// Docker's own convention is reverse-DNS for vendor labels, so we sit
/// under `io.botworkz.*`. session-broker stamps every spawned container
/// with `io.botworkz.tenant`, `io.botworkz.workspace`,
/// `io.botworkz.plugin` (RFE #105 round-3) — and the future janitor
/// will read those keys back via `docker inspect` to bisect
/// docker-vs-DB drift when reconciling routing state on broker
/// restart.
///
/// We enforce the prefix at the launcher's wire boundary (not on
/// session-broker's outbound side) because the launcher is the trust
/// gate the session-broker side talks through: an arbitrary
/// caller-supplied label namespace would expand the label-key
/// search surface the janitor has to walk on every recovery cycle.
/// One namespace = one grep, future-proof.
pub const LABEL_NAMESPACE_PREFIX: &str = "io.botworkz.";

#[derive(Clone, Debug)]
pub struct Validators {
    name_re: Regex,
    image_re: Regex,
    network_re: Regex,
    staging_path_re: Regex,
    agent_dir_re: Regex,
    staging_base: String,
    agents_base: String,
}

impl Validators {
    pub fn new(image_allowlist_regex: &str) -> Result<Self, String> {
        Self::new_with_bases(image_allowlist_regex, STAGING_BASE, AGENTS_BASE)
    }

    /// Construct validators with custom base paths.  Used by `new` for
    /// production and exposed as `pub(crate)` for unit tests that need
    /// staging/agent paths under a tempdir rather than the production
    /// `/var/lib/botwork/tenants` tree.
    pub(crate) fn new_with_bases(
        image_allowlist_regex: &str,
        staging_base: &str,
        agents_base: &str,
    ) -> Result<Self, String> {
        let escaped_staging = regex::escape(staging_base);
        let escaped_agents = regex::escape(agents_base);
        let name_re = Regex::new(r"^mcp_session_[a-f0-9]{12}$").map_err(|err| err.to_string())?;
        let image_re = Regex::new(image_allowlist_regex).map_err(|err| err.to_string())?;
        let network_re = Regex::new(r"^[a-z0-9_-]+$").map_err(|err| err.to_string())?;
        let staging_path_re = Regex::new(&format!(
            r"^{escaped_staging}/{TENANT_RE}/staging/[a-f0-9]{{12}}$"
        ))
        .map_err(|err| err.to_string())?;
        let agent_dir_re = Regex::new(&format!(
            // Workspace segment shares the tenant character class (lowercase,
            // digits, hyphens, 1-31 chars). Reusing TENANT_RE here; introduce
            // a separate WORKSPACE_RE if the rules diverge.
            //
            // The producer side (session-broker's ext_proc.rs::agent_dir) writes
            //   /var/lib/botwork/tenants/<t>/workspaces/<w>/agents/<id>
            // since RFE #101 PR2 renamed `namespace` -> `workspace` across the
            // session/control plane. The validator MUST track that rename: an
            // out-of-sync regex here causes every /bind-agent POST to 400 with
            // "invalid agent_dir", which silently breaks the per-agent
            // workspace bind mount — `fs write` and `exec-bash cat` then look
            // at disjoint container-local `/workspace` directories and the
            // cross-plugin assertion in the smoke harness fails on missing
            // shared file content rather than on the actual root cause.
            r"^{escaped_agents}/{TENANT_RE}/workspaces/{TENANT_RE}/agents/[A-Za-z0-9_-]{{1,64}}$"
        ))
        .map_err(|err| err.to_string())?;

        Ok(Self {
            name_re,
            image_re,
            network_re,
            staging_path_re,
            agent_dir_re,
            staging_base: staging_base.to_string(),
            agents_base: agents_base.to_string(),
        })
    }

    pub fn valid_name(&self, value: &str) -> bool {
        self.name_re.is_match(value)
    }

    pub fn valid_image(&self, value: &str) -> bool {
        self.image_re.is_match(value)
    }

    pub fn valid_network(&self, value: &str) -> bool {
        self.network_re.is_match(value)
    }

    pub fn valid_staging_path(&self, value: &str) -> bool {
        self.staging_path_re.is_match(value)
    }

    pub fn valid_agent_dir(&self, value: &str) -> bool {
        self.agent_dir_re.is_match(value)
    }

    pub fn valid_env_name(&self, name: &str) -> bool {
        valid_env_name(name)
    }

    /// RFE #105 round-3: gate caller-supplied docker label keys.
    ///
    /// Rules:
    /// * starts with [`LABEL_NAMESPACE_PREFIX`] (`io.botworkz.`);
    /// * the trailing segment matches `[a-z][a-z0-9_-]*` — the
    ///   character class docker itself accepts, plus the
    ///   lowercase-only rule we apply to env names so the iden
    ///   policy stays uniform across the surfaces a caller can
    ///   stamp on a container;
    /// * total length capped at 128 bytes (well under docker's
    ///   own label-key cap; same shape we use for image refs).
    ///
    /// The trust posture is "caller can pick the trailing
    /// segment, launcher pins the namespace". This keeps the
    /// future janitor's recovery `docker inspect | grep
    /// io.botworkz.` cheap and bisectable.
    pub fn valid_label_name(&self, name: &str) -> bool {
        valid_label_name(name)
    }

    pub fn safe_staging_path(&self, value: &str) -> Result<String, LauncherError> {
        if !self.valid_staging_path(value) {
            return Err(LauncherError::BadRequest(
                "invalid staging_path".to_string(),
            ));
        }
        let safe = normalize_path(value);
        if !safe.starts_with(&format!("{}/", self.staging_base)) {
            return Err(LauncherError::BadRequest(
                "invalid staging_path".to_string(),
            ));
        }
        Ok(safe)
    }

    pub fn safe_agent_dir(&self, value: &str) -> Result<String, LauncherError> {
        if !self.valid_agent_dir(value) {
            return Err(LauncherError::BadRequest("invalid agent_dir".to_string()));
        }
        let safe = normalize_path(value);
        if !safe.starts_with(&format!("{}/", self.agents_base)) {
            return Err(LauncherError::BadRequest("invalid agent_dir".to_string()));
        }
        Ok(safe)
    }
}

/// Returns `true` for env var names that carry sensitive data and must never
/// appear on a subprocess argv.  The `BOTWORK_SECRET_` prefix is the project's
/// canonical contract for secret-bearing env vars.
///
/// This contract is enforced in three places that must stay in sync:
///   * `launcher/src/validate.rs::is_sensitive_env` (this file): which env
///     values get routed to docker via stdin instead of argv.
///   * `config-broker/src/registry.rs`: rejects user-supplied static
///     env entries that use this prefix (reserved for vault-derived values).
///   * `session-broker/src/secrets.rs::SECRET_ENV_PREFIX`: where the broker
///     stamps secrets it fetched from the auth-broker before forwarding to
///     the launcher.
///
/// Changing the prefix or adding a second sensitive prefix requires updating
/// all three call sites.
pub fn is_sensitive_env(name: &str) -> bool {
    name.starts_with("BOTWORK_SECRET_")
}

/// RFE #105 round-3: see [`Validators::valid_label_name`] for the
/// rationale. Free function so the launcher's wire-layer payload
/// parser can call it without holding a `Validators` instance, and so
/// the tests can exercise it directly.
pub fn valid_label_name(name: &str) -> bool {
    const MAX_LABEL_NAME_LEN: usize = 128;
    if name.len() > MAX_LABEL_NAME_LEN {
        return false;
    }
    let Some(suffix) = name.strip_prefix(LABEL_NAMESPACE_PREFIX) else {
        return false;
    };
    // Trailing segment: lowercase ASCII alpha to start, then any
    // mix of lowercase alpha / digits / `_` / `-`. Empty suffix is
    // explicitly rejected — `io.botworkz.` on its own is not a key.
    let bytes = suffix.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !first.is_ascii_lowercase() {
        return false;
    }
    bytes
        .iter()
        .skip(1)
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

pub fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let first = bytes[0];
    if !(first.is_ascii_uppercase() || first == b'_') {
        return false;
    }

    if bytes
        .iter()
        .skip(1)
        .any(|byte| !(byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_'))
    {
        return false;
    }

    if RESERVED_ENV_NAMES.contains(&name) {
        return false;
    }

    // Block all Docker-related env overrides in addition to specific reserved names.
    !name.starts_with("DOCKER_")
}

fn normalize_path(path: &str) -> String {
    let mut normalized = PathBuf::new();

    for component in Path::new(path).components() {
        match component {
            Component::RootDir => normalized.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => {}
        }
    }

    normalized.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        valid_env_name, valid_label_name, Validators, LABEL_NAMESPACE_PREFIX, RESERVED_ENV_NAMES,
    };
    use crate::error::LauncherError;

    fn validators() -> Validators {
        Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators")
    }

    #[test]
    fn validators_accept_expected_values() {
        let validators = validators();

        assert!(validators.valid_name("mcp_session_aabbccddeeff"));
        assert!(validators.valid_image("botwork/mcp-echo:local"));
        assert!(validators.valid_image("botwork/mcp-echo:local"));
        assert!(validators.valid_network("botwork_network-1"));
        assert!(validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddeeff"));
        assert!(validators
            .valid_agent_dir("/var/lib/botwork/tenants/acme/workspaces/mcp/agents/my_agent-1"));
    }

    #[test]
    fn validators_reject_obvious_bad_values() {
        let validators = validators();

        assert!(!validators.valid_name("mcp_session_abcdef"));
        assert!(!validators.valid_image("botspace/mcp-echo:local"));
        assert!(!validators.valid_image("ghcr.io/phlax/botwork:latest"));
        assert!(!validators.valid_image("foo/mcp-echo:local"));
        assert!(!validators.valid_network("bad network"));
        assert!(
            !validators.valid_staging_path("/var/lib/botwork/tenants/Acme/staging/aabbccddeeff")
        );
        assert!(!validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddee"));
        assert!(
            !validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/../aabbccddeeff")
        );
        assert!(!validators.valid_agent_dir("/var/lib/botwork/tenants/acme/agents/invalid.agent"));
        assert!(!validators.valid_agent_dir("/tmp/agents/agentA"));
        assert!(!validators
            .valid_agent_dir("/var/lib/botwork/tenants/acme/workspaces/mcp/agents/invalid.agent"));
    }

    #[test]
    fn safe_paths_normalize_without_symlink_resolution() {
        let validators = validators();

        let staging = validators
            .safe_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddeeff")
            .expect("staging path should validate");
        assert_eq!(
            staging,
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff"
        );

        let agent = validators
            .safe_agent_dir("/var/lib/botwork/tenants/acme/workspaces/mcp/agents/agent_A")
            .expect("agent dir should validate");
        assert_eq!(
            agent,
            "/var/lib/botwork/tenants/acme/workspaces/mcp/agents/agent_A"
        );
    }

    #[test]
    fn is_sensitive_env_classifies_by_prefix() {
        use super::is_sensitive_env;

        assert!(is_sensitive_env("BOTWORK_SECRET_GITHUB_COM_PAT"));
        assert!(is_sensitive_env("BOTWORK_SECRET_"));
        assert!(!is_sensitive_env("BOTWORK_NOT_SECRET"));
        assert!(!is_sensitive_env("FOO"));
        assert!(!is_sensitive_env(""));
    }

    #[test]
    fn valid_env_name_enforces_shape_and_reserved_names() {
        assert!(valid_env_name("BOTWORK_SECRET_GITHUB_COM_PAT"));
        assert!(valid_env_name("_BOTWORK_SECRET_1"));

        assert!(!valid_env_name(""));
        assert!(!valid_env_name("botwork_secret"));
        assert!(!valid_env_name("BOTWORK-SECRET"));
        assert!(!valid_env_name("1BOTWORK_SECRET"));
        assert!(!valid_env_name("BOTWORK=SECRET"));
        assert!(!valid_env_name("BOTWORK_\0_SECRET"));
        assert!(!valid_env_name("DOCKER_SECRET"));

        for name in RESERVED_ENV_NAMES {
            assert!(!valid_env_name(name));
        }
    }

    #[test]
    fn valid_env_name_accepts_home_and_user() {
        assert!(valid_env_name("HOME"));
        assert!(valid_env_name("USER"));
    }

    #[test]
    fn validators_expose_valid_env_name() {
        let validators = validators();
        assert!(validators.valid_env_name("BOTWORK_SECRET"));
        assert!(!validators.valid_env_name("PATH"));
    }

    // ── RFE #105 round-3: docker label validator ────────────────────────────

    #[test]
    fn valid_label_name_accepts_namespaced_keys() {
        // The three keys session-broker will stamp on every spawn.
        assert!(valid_label_name("io.botworkz.tenant"));
        assert!(valid_label_name("io.botworkz.workspace"));
        assert!(valid_label_name("io.botworkz.plugin"));
        // Trailing segment may use the full lowercase / digit / `_` / `-`
        // character class — the validator must not gate that.
        assert!(valid_label_name("io.botworkz.foo_bar"));
        assert!(valid_label_name("io.botworkz.foo-bar"));
        assert!(valid_label_name("io.botworkz.foo1"));
        assert!(valid_label_name("io.botworkz.a"));
    }

    #[test]
    fn valid_label_name_rejects_unprefixed_keys() {
        // Plain docker conventions and other reverse-DNS namespaces
        // must be refused — we own io.botworkz.* exclusively.
        assert!(!valid_label_name("tenant"));
        assert!(!valid_label_name("com.docker.compose.project"));
        assert!(!valid_label_name("botwork.tenant"));
        // Missing the trailing dot in the prefix is a different
        // namespace ("io.botworkz" is not a key, and "io.botworkzx.*"
        // is a sibling namespace we don't own).
        assert!(!valid_label_name("io.botworkz"));
        assert!(!valid_label_name("io.botworkzx.tenant"));
    }

    #[test]
    fn valid_label_name_rejects_empty_suffix() {
        // `io.botworkz.` on its own is not a key (the prefix is a
        // namespace, not a key in itself).
        assert!(!valid_label_name("io.botworkz."));
    }

    #[test]
    fn valid_label_name_rejects_bad_suffix_chars() {
        // Suffix must START with a lowercase letter.
        assert!(!valid_label_name("io.botworkz.1tenant"));
        assert!(!valid_label_name("io.botworkz._tenant"));
        assert!(!valid_label_name("io.botworkz.-tenant"));
        // Uppercase is a uniform no across env/label/iden rules.
        assert!(!valid_label_name("io.botworkz.Tenant"));
        assert!(!valid_label_name("io.botworkz.TENANT"));
        // No dots in the suffix — the prefix already encodes the
        // dotted namespace, an internal dot would imply a new
        // sub-namespace we haven't reserved.
        assert!(!valid_label_name("io.botworkz.foo.bar"));
        // Whitespace, null, equals — all docker-side hazards.
        assert!(!valid_label_name("io.botworkz.foo bar"));
        assert!(!valid_label_name("io.botworkz.foo\0bar"));
        assert!(!valid_label_name("io.botworkz.foo=bar"));
    }

    #[test]
    fn valid_label_name_enforces_length_cap() {
        // 128 bytes total. Prefix is 12 chars, so suffix can be 116.
        let suffix_ok = "a".repeat(116);
        let key_ok = format!("{LABEL_NAMESPACE_PREFIX}{suffix_ok}");
        assert_eq!(key_ok.len(), 128);
        assert!(valid_label_name(&key_ok));

        let suffix_overflow = "a".repeat(117);
        let key_too_long = format!("{LABEL_NAMESPACE_PREFIX}{suffix_overflow}");
        assert_eq!(key_too_long.len(), 129);
        assert!(!valid_label_name(&key_too_long));
    }

    #[test]
    fn validators_expose_valid_label_name() {
        let validators = validators();
        assert!(validators.valid_label_name("io.botworkz.tenant"));
        assert!(!validators.valid_label_name("tenant"));
    }

    #[test]
    fn safe_staging_and_agent_paths_reject_invalid_values() {
        let validators = validators();
        let staging_err = validators
            .safe_staging_path("/outside/acme/staging/aabbccddeeff")
            .expect_err("invalid staging path should fail");
        assert!(matches!(staging_err, LauncherError::BadRequest(_)));

        let agent_err = validators
            .safe_agent_dir("/outside/acme/workspaces/ws/agents/agent-1")
            .expect_err("invalid agent dir should fail");
        assert!(matches!(agent_err, LauncherError::BadRequest(_)));
    }

    #[test]
    fn validators_new_rejects_invalid_image_regex() {
        let err = Validators::new("(").expect_err("invalid regex should fail");
        assert!(!err.is_empty(), "regex error must be surfaced");
    }

    // ── additional coverage for uncovered branches ──────────────────

    #[test]
    fn normalize_path_handles_curdir_and_parentdir_components() {
        // CurDir (.) is skipped (line 246)
        assert_eq!(
            super::normalize_path("/tmp/./foo"),
            "/tmp/foo",
            "single CurDir component"
        );
        assert_eq!(
            super::normalize_path("/tmp/./././foo"),
            "/tmp/foo",
            "repeated CurDir components"
        );

        // ParentDir (..) pops the last pushed component (lines 247-249)
        assert_eq!(
            super::normalize_path("/tmp/foo/../bar"),
            "/tmp/bar",
            "single ParentDir"
        );
        assert_eq!(
            super::normalize_path("/tmp/a/b/../../c"),
            "/tmp/c",
            "double ParentDir"
        );
        assert_eq!(
            super::normalize_path("/tmp/./foo/../bar"),
            "/tmp/bar",
            "mixed CurDir and ParentDir"
        );
    }

    #[test]
    fn safe_staging_path_second_check_rejects_when_normalize_escapes_base() {
        // Use a trailing-slash staging_base so that:
        //   • the regex encodes a double-slash (base + "/" in the format string)
        //   • a path that matches (double-slash) normalizes to a single-slash
        //     path that no longer starts_with the double-slash prefix
        // → the SECOND error branch (lines 145-148) fires.
        let trailing = "/tmp/staging_trailing/";
        let validators = Validators::new_with_bases(
            r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$",
            trailing,
            "/var/lib/botwork/tenants", // agents_base irrelevant here
        )
        .expect("validators");

        // Double-slash path passes the regex (which embeds the trailing slash)
        let double_slash_path = "/tmp/staging_trailing//acme/staging/aabbccddeeff";
        let result = validators.safe_staging_path(double_slash_path);
        assert!(
            matches!(result, Err(LauncherError::BadRequest(_))),
            "expected second-check error, got {result:?}"
        );
    }

    #[test]
    fn safe_agent_dir_second_check_rejects_when_normalize_escapes_base() {
        // Same trick for agents_base (lines 157-160 in safe_agent_dir).
        let trailing = "/tmp/agents_trailing/";
        let validators = Validators::new_with_bases(
            r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$",
            "/var/lib/botwork/tenants", // staging_base irrelevant here
            trailing,
        )
        .expect("validators");

        // The agent_dir_re encodes trailing + "/" → double slash.
        let double_slash_path = "/tmp/agents_trailing//acme/workspaces/mcp/agents/agent-abc123";
        let result = validators.safe_agent_dir(double_slash_path);
        assert!(
            matches!(result, Err(LauncherError::BadRequest(_))),
            "expected second-check error, got {result:?}"
        );
    }
}
