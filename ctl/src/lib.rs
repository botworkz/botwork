pub mod bootstrap;
pub mod cli;
pub mod mcp_probe;
pub mod ps;

pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

/// Trivial passthrough to `botwork_version::format_full`.
/// NOT covered by offline unit tests (GIT_SHA embed is build-time wiring).
#[cfg(not(tarpaulin_include))]
pub fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

/// Process-args entry point — reads `std::env::args()` and delegates.
/// NOT covered by offline unit tests (cannot inject process args).
#[cfg(not(tarpaulin_include))]
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
