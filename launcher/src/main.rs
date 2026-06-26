use std::io::Write;

use botwork_launcher::{run, PREFIX};

fn handle_version_flag(args: &[String], mut writer: impl Write) -> Option<i32> {
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            writeln!(writer, "botwork-launcher {}", botwork_version::full())
                .expect("write version");
            Some(0)
        }
        _ => None,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = handle_version_flag(&args, std::io::stdout()) {
        std::process::exit(code);
    }

    if let Err(err) = run().await {
        eprintln!("{PREFIX} {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::handle_version_flag;

    #[test]
    fn version_flags_print_the_shared_version() {
        for flag in ["--version", "-V"] {
            let mut output = Vec::new();
            let args = vec!["botwork-launcher".to_string(), flag.to_string()];
            assert_eq!(handle_version_flag(&args, &mut output), Some(0));
            assert_eq!(
                String::from_utf8(output).expect("utf8"),
                format!("botwork-launcher {}\n", botwork_version::full())
            );
        }
    }
}
