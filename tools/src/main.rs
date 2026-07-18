use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if matches!(
        args.get(1).map(String::as_str),
        Some("--version") | Some("-V")
    ) {
        writeln!(
            std::io::stdout(),
            "botwork-tools {}",
            botwork_tools::version_string()
        )
        .expect("failed to write version output");
        std::process::exit(0);
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    std::process::exit(botwork_tools::run_with_args(args));
}
