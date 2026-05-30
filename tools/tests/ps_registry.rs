use botwork_tools::ps::registry::load_registry;

#[test]
fn loads_registry_and_session_entries() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("registry_valid.json");

    let registry = load_registry(&path).unwrap();
    assert_eq!(registry.version, 1);
    assert_eq!(registry.updated_at, "2026-05-20T00:00:00Z");

    let session = registry.sessions.get("mcp_session_a").unwrap();
    assert_eq!(session.container, "mcp_session_a");
    assert_eq!(session.staging_path, "/tmp/staging/a");
    assert_eq!(session.mcp_session_id, None);
    assert_eq!(session.agent_id, None);
    assert_eq!(session.image, "ghcr.io/phlax/image:a");
    assert_eq!(session.created_at, "2026-05-20T00:00:00Z");
    assert_eq!(session.bound_at, None);
}

#[test]
fn defaults_missing_sessions_map_to_empty() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("registry_missing_sessions.json");

    let registry = load_registry(&path).unwrap();
    assert!(registry.sessions.is_empty());
}
