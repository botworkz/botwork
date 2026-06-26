fn main() {
    println!("cargo:rerun-if-changed=../VERSION");
    println!("cargo:rerun-if-env-changed=BOTWORK_GIT_SHA");
}
