use grev::git_revision_auto;

fn main() {
    let rev = git_revision_auto(".")
        .unwrap_or(None)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_REVISION={}", rev);
}
