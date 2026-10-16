use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Commits, branch switches, and new tags all change the described version.
    // A missing path would make cargo rerun this script on every build.
    for path in ["HEAD", "refs", "packed-refs"] {
        if let Some(git_path) = git(&["rev-parse", "--git-path", path])
            && std::path::Path::new(&git_path).exists()
        {
            println!("cargo:rerun-if-changed={git_path}");
        }
    }

    // The update check parses this as semver, so a bare commit hash (no tags
    // reachable) is carried as build metadata on the package version.
    let package_version = env!("CARGO_PKG_VERSION");
    let version = match git(&[
        "describe",
        "--tags",
        "--match",
        "v[0-9]*.[0-9]*.[0-9]*",
        "--always",
    ]) {
        Some(described) if described.starts_with('v') => described,
        Some(hash) => format!("v{package_version}+{hash}"),
        None => format!("v{package_version}"),
    };
    println!("cargo:rustc-env=INXM_BUILD_VERSION={version}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/favicon.ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        res.compile().expect("embed Windows icon resource");
    }
}
