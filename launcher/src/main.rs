use botwork_launcher::{run, PREFIX};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{PREFIX} {err}");
        std::process::exit(1);
    }
}
