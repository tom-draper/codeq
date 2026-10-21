use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Clap's generated command parser exceeds MSVC's default 1 MiB main stack in debug builds.
        println!("cargo:rustc-link-arg-bin=codeq=/STACK:8388608");
    }
    println!("cargo:rerun-if-env-changed=CODEQ_BUILD_REVISION");
    let mut refs = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Ok(output) = Command::new("git").args(["symbolic-ref", "HEAD"]).output() {
        if output.status.success() {
            refs.push(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
    }
    for reference in refs {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", &reference])
            .output()
        {
            if output.status.success() {
                println!(
                    "cargo:rerun-if-changed={}",
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
        }
    }
    let revision = std::env::var("CODEQ_BUILD_REVISION")
        .ok()
        .or_else(|| {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CODEQ_BUILD_REVISION={revision}");
}
