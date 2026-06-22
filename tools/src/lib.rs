pub mod bootstrap;
pub mod cli;
pub mod ps;

pub fn run() -> i32 {
    match cli::dispatch(std::env::args().collect()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code()
        }
    }
}
