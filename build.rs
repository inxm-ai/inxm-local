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
    bundle_documentation().expect("bundle offline documentation");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/favicon.ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        res.compile().expect("embed Windows icon resource");
    }
}

fn bundle_documentation() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=mkdocs.yml");
    println!("cargo:rerun-if-changed=docs");
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let config: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(manifest.join("mkdocs.yml"))?)?;
    let online = config["site_url"]
        .as_str()
        .ok_or("mkdocs site_url is required")?;
    let mut pages = Vec::new();
    collect_document_pages(&config["nav"], "", &mut pages)?;
    let mut generated = format!(
        "pub const DOC_ONLINE_BASE: &str = {online:?};\npub const DOC_PAGES: &[(&str, &str, &str)] = &[\n"
    );
    for (title, relative) in pages {
        let path = manifest.join("docs").join(&relative);
        if !path.is_file() {
            return Err(format!("missing bundled document: {relative}").into());
        }
        generated.push_str(&format!(
            "({title:?}, {relative:?}, include_str!({:?})),\n",
            path
        ));
    }
    generated.push_str("];\n");
    let output = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("documentation.rs");
    std::fs::write(output, generated)?;
    Ok(())
}

fn collect_document_pages(
    nav: &serde_yaml::Value,
    prefix: &str,
    pages: &mut Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let items = nav
        .as_sequence()
        .ok_or("documentation nav must be a sequence")?;
    for item in items {
        let entries = item
            .as_mapping()
            .ok_or("documentation nav entry must be a mapping")?;
        for (title, target) in entries {
            let title = title
                .as_str()
                .ok_or("documentation title must be a string")?;
            let title = if prefix.is_empty() {
                title.to_owned()
            } else {
                format!("{prefix} / {title}")
            };
            if let Some(path) = target.as_str() {
                let path_buf = std::path::Path::new(path);
                if path_buf.is_absolute()
                    || path_buf
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(format!("invalid documentation path: {path}").into());
                }
                pages.push((title, path.to_owned()));
            } else {
                collect_document_pages(target, &title, pages)?;
            }
        }
    }
    Ok(())
}
