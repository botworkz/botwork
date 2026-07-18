use botctl::ps::render::{render_table, TableRow};

#[test]
fn renders_header_only_when_no_rows() {
    let output = render_table(&[]);
    assert_eq!(
        output,
        "ID             CONTAINER                   AGENT                IMAGE                       AGE\n"
    );
}

#[test]
fn renders_id_container_agent_image_and_age_columns() {
    let output = render_table(&[TableRow {
        id: "f2cc95e9f1a3".to_string(),
        container: "mcp_session_example".to_string(),
        agent: "agent_123".to_string(),
        image: "ghcr.io/phlax/session:latest".to_string(),
        age: "2 hours ago".to_string(),
    }]);

    let mut lines = output.lines();
    assert_eq!(
        lines.next().unwrap(),
        "ID             CONTAINER                   AGENT                IMAGE                       AGE"
    );

    let row = lines.next().unwrap();
    assert!(row.starts_with("f2cc95e9f1a3  "));
    assert!(row.contains(" mcp_session_example         "));
    assert!(row.contains(" agent_123            "));
    assert!(row.contains(" ghcr.io/phlax/session:latest "));
    assert!(row.ends_with("2 hours ago"));
    assert!(lines.next().is_none());
}
