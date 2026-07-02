use std::path::PathBuf;

#[path = "../src/config.rs"]
#[allow(dead_code)]
mod config;

use config::{Config, Project, RepoMap, slug_from_url};

#[test]
fn slug_from_url_ssh_e_https() {
    assert_eq!(
        slug_from_url("git@github.com:tono-lang/parser.git").as_deref(),
        Some("tono-lang/parser")
    );
    assert_eq!(
        slug_from_url("https://github.com/tono-lang/runtime").as_deref(),
        Some("tono-lang/runtime")
    );
    assert_eq!(
        slug_from_url("https://github.com/tono-lang/runtime.git").as_deref(),
        Some("tono-lang/runtime")
    );
}

#[test]
fn repo_map_constroi_slug_para_path() {
    let project = Project {
        name: "x".into(),
        root: PathBuf::from("/p"),
        backlog: PathBuf::from("/p/.backlog"),
        docs: PathBuf::from("/p/docs"),
        work_dir: PathBuf::from("/p/.jaum"),
        repos: vec![
            RepoMap {
                slug: "org/a".into(),
                path: PathBuf::from("/repos/a"),
            },
            RepoMap {
                slug: "org/b".into(),
                path: PathBuf::from("/repos/b"),
            },
        ],
    };
    let map = project.repo_map();
    assert_eq!(map.get("org/a"), Some(&PathBuf::from("/repos/a")));
    assert_eq!(map.get("org/b"), Some(&PathBuf::from("/repos/b")));
}

#[test]
fn config_toml_roundtrip() {
    let cfg = Config {
        projects: vec![Project {
            name: "tono".into(),
            root: PathBuf::from("/p"),
            backlog: PathBuf::from("/p/.backlog"),
            docs: PathBuf::from("/p/docs"),
            work_dir: PathBuf::from("/p/.jaum"),
            repos: vec![RepoMap {
                slug: "tono/parser".into(),
                path: PathBuf::from("/repos/parser"),
            }],
        }],
    };
    let toml = toml::to_string_pretty(&cfg).unwrap();
    let back: Config = toml::from_str(&toml).unwrap();
    assert_eq!(back.projects.len(), 1);
    assert_eq!(back.projects[0].name, "tono");
    assert_eq!(back.projects[0].repos[0].slug, "tono/parser");
    assert_eq!(back.find("tono").map(|p| p.name.as_str()), Some("tono"));
}
