#![forbid(unsafe_code)]

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CASTVANE_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{reference}");
    }

    let commit = std::env::var("CASTVANE_BUILD_COMMIT").ok().or_else(|| {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    });
    let commit = commit
        .as_deref()
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    println!(
        "cargo:rustc-env=CASTVANE_BUILD_COMMIT={}",
        commit.unwrap_or("unknown")
    );
}
