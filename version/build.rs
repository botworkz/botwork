fn main() {
    // BOTWORK_GIT_SHA is read via `option_env!` in lib.rs; tell cargo
    // to retrigger a rebuild whenever the env var changes so a fresh
    // CI build picks up a new sha without a `cargo clean`.
    println!("cargo:rerun-if-env-changed=BOTWORK_GIT_SHA");
}
