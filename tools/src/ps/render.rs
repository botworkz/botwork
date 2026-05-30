pub const ID_WIDTH: usize = 14;
pub const CONTAINER_WIDTH: usize = 27;
pub const AGENT_WIDTH: usize = 20;
pub const IMAGE_WIDTH: usize = 27;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub id: String,
    pub container: String,
    pub agent: String,
    pub image: String,
    pub age: String,
}

pub fn render_table(rows: &[TableRow]) -> String {
    let mut output = String::new();
    output.push_str(&format_row("ID", "CONTAINER", "AGENT", "IMAGE", "AGE"));
    output.push('\n');

    for row in rows {
        output.push_str(&format_row(
            &row.id,
            &row.container,
            &row.agent,
            &row.image,
            &row.age,
        ));
        output.push('\n');
    }

    output
}

fn format_row(id: &str, container: &str, agent: &str, image: &str, age: &str) -> String {
    format!(
        "{id:<ID_WIDTH$} {container:<CONTAINER_WIDTH$} {agent:<AGENT_WIDTH$} {image:<IMAGE_WIDTH$} {age}",
    )
}
