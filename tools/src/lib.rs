pub mod bootstrap;
pub mod cli;
pub mod frontdoor;
pub mod mcp_probe;
pub mod ps;

pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

pub fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

pub fn run() -> i32 {
    match cli::dispatch(std::env::args().collect()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code()
        }
    }
}
