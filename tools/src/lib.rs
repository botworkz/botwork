pub mod bootstrap;
pub mod cli;
pub mod mcp_probe;
pub mod ps;

pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

pub fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

pub fn run() -> i32 {
    run_with_args(std::env::args().collect())
}

pub fn run_with_args(args: Vec<String>) -> i32 {
    match cli::dispatch(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code()
        }
    }
}
