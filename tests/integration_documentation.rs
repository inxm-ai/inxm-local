//! Keep offline documentation and the MkDocs website on the same source/navigation.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use inxm_local::app::documentation::DOC_PAGES;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

fn nav_pages(nav: &serde_yaml::Value, prefix: &str, result: &mut Vec<(String, String)>) {
    for entry in nav.as_sequence().expect("nav sequence") {
        for (title, target) in entry.as_mapping().expect("nav mapping") {
            let title = title.as_str().expect("nav title");
            let title = if prefix.is_empty() {
                title.to_owned()
            } else {
                format!("{prefix} / {title}")
            };
            if let Some(path) = target.as_str() {
                result.push((title, path.to_owned()));
            } else {
                nav_pages(target, &title, result);
            }
        }
    }
}

#[test]
fn bundled_pages_match_website_navigation_and_original_markdown() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../mkdocs.yml")).expect("MkDocs configuration");
    let mut expected = Vec::new();
    nav_pages(&config["nav"], "", &mut expected);
    let actual = DOC_PAGES
        .iter()
        .map(|(title, path, _)| ((*title).to_owned(), (*path).to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "offline order/titles must follow MkDocs");
    let unique = DOC_PAGES
        .iter()
        .map(|(_, path, _)| *path)
        .collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), DOC_PAGES.len(), "duplicate navigation target");
    for (_, path, bundled) in DOC_PAGES {
        assert_eq!(
            *bundled,
            std::fs::read_to_string(root.join("docs").join(path)).expect("source document"),
            "bundled source differs: {path}"
        );
    }
}

fn local_target(source: &str, destination: &str) -> Option<(String, Option<String>)> {
    if destination.contains(":") || destination.starts_with("//") {
        return None;
    }
    let (target, fragment) = destination
        .split_once('#')
        .map_or((destination, None), |(path, anchor)| {
            (path, Some(anchor.to_owned()))
        });
    let target = target.split('?').next().unwrap_or(target);
    if target.is_empty() {
        return Some((source.to_owned(), fragment));
    }
    let mut resolved = PathBuf::new();
    let base = if target.starts_with('/') {
        PathBuf::from(target.trim_start_matches('/'))
    } else {
        Path::new(source)
            .parent()
            .unwrap_or(Path::new(""))
            .join(target)
    };
    for component in base.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir => assert!(resolved.pop(), "link escapes docs: {destination}"),
            Component::CurDir => {}
            _ => panic!("unsupported link: {destination}"),
        }
    }
    Some((resolved.to_string_lossy().replace('\\', "/"), fragment))
}

fn heading_anchors(markdown: &str) -> BTreeSet<String> {
    let html_heading = regex::Regex::new(r"(?m)<h([1-6])>(.*?)</h[1-6]>").unwrap();
    let markdown = html_heading.replace_all(markdown, |captures: &regex::Captures<'_>| {
        format!(
            "{} {}",
            "#".repeat(captures[1].parse().unwrap()),
            &captures[2]
        )
    });
    let mut anchors = BTreeSet::new();
    let mut heading = None::<String>;
    for event in Parser::new_ext(&markdown, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading) = &mut heading {
                    heading.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let slug = heading
                    .take()
                    .unwrap()
                    .to_lowercase()
                    .chars()
                    .filter_map(|character| {
                        if character.is_alphanumeric() || character == '_' || character == '-' {
                            Some(character)
                        } else if character.is_whitespace() {
                            Some('-')
                        } else {
                            None
                        }
                    })
                    .collect::<String>();
                anchors.insert(slug);
            }
            _ => {}
        }
    }
    anchors
}

#[test]
fn internal_links_resolve_to_bundled_pages_and_existing_headings() {
    for (_, source, markdown) in DOC_PAGES {
        for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
            let Event::Start(Tag::Link { dest_url, .. }) = event else {
                continue;
            };
            let Some((target, fragment)) = local_target(source, &dest_url) else {
                continue;
            };
            let (_, _, target_source) = DOC_PAGES
                .iter()
                .find(|(_, path, _)| *path == target)
                .unwrap_or_else(|| panic!("{source}: link {dest_url} is not bundled"));
            if let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) {
                assert!(
                    heading_anchors(target_source).contains(&fragment),
                    "{source}: link {dest_url} has no matching heading"
                );
            }
        }
    }
}
