use purgery_core::ClientConfig;

const PREFIX: &str = r#"
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"
"#;

fn parse(body: &str) -> Result<ClientConfig, purgery_core::ConfigError> {
    ClientConfig::from_toml(&format!("{PREFIX}\n{body}"))
}

fn valid_roots() -> &'static str {
    r#"
[[root]]
name = "videos"
path = "/home/user/Videos"

[[root]]
name = "configs"
path = "/home/user/my/server-configs"
"#
}

#[test]
fn issue_34_accepts_multiple_client_roots() {
    assert!(parse(valid_roots()).is_ok());
}

#[test]
fn issue_34_rejects_missing_client_roots() {
    assert!(parse("").is_err());
}

#[test]
fn issue_34_rejects_duplicate_client_root_names() {
    assert!(parse(
        r#"
[[root]]
name = "videos"
path = "/one"
[[root]]
name = "videos"
path = "/two"
"#
    )
    .is_err());
}

#[test]
fn issue_34_validates_client_root_names_and_paths() {
    for invalid in [
        r#"[[root]]
name = ""
path = "/absolute""#,
        r#"[[root]]
name = "bad/name"
path = "/absolute""#,
        r#"[[root]]
name = "videos"
path = "relative""#,
    ] {
        assert!(parse(invalid).is_err(), "accepted invalid root: {invalid}");
    }
    assert!(parse(
        r#"[[root]]
name = "videos_2"
path = "/absolute""#
    )
    .is_ok());
}

#[test]
fn issue_34_accepts_root_qualified_from_and_to() {
    let body = format!(
        r#"{}
[[sync]]
from = "videos/cats"
to = "univ/videos/cats"
"#,
        valid_roots()
    );
    let config = parse(&body).unwrap();
    assert_eq!(
        config.sync[0].source.qualified_path().as_str(),
        "videos/cats"
    );
    assert_eq!(config.sync[0].from_path.as_str(), "/home/user/Videos/cats");
}

#[test]
fn issue_34_rejects_invalid_and_unknown_client_sources() {
    for source in [
        "",
        "/videos",
        ".",
        "./videos",
        "videos/../cats",
        "videos//cats",
        "unknown/cats",
    ] {
        let body = format!(
            r#"{}
[[sync]]
from = "{source}"
to = "univ/videos"
"#,
            valid_roots()
        );
        assert!(parse(&body).is_err(), "accepted invalid source: {source}");
    }
}

#[test]
fn issue_34_accepts_match_and_postprocess_list() {
    let body = format!(
        r#"{}
[[sync]]
from = "videos"
to = "univ/videos"
match = "**/*dog*.png"
postprocess = ["compress-image"]
delete_after_import = true
"#,
        valid_roots()
    );
    assert!(parse(&body).is_ok());
}

#[test]
fn issue_34_rejects_invalid_postprocess_selection() {
    for fields in [
        "postprocess = []\ndelete_after_import = true",
        "postprocess = \"compress-image\"\ndelete_after_import = true",
        "postprocess = [\"compress-image\"]",
    ] {
        let body = format!(
            r#"{}
[[sync]]
from = "videos"
to = "univ/videos"
{fields}
"#,
            valid_roots()
        );
        assert!(parse(&body).is_err(), "accepted invalid fields: {fields}");
    }
}

#[test]
fn issue_34_rejects_user_authored_sync_name() {
    let body = format!(
        r#"{}
[[sync]]
name = "dogs"
from = "videos"
to = "univ/videos"
"#,
        valid_roots()
    );
    assert!(parse(&body).is_err());
}

#[test]
fn issue_34_generates_deterministic_sync_ids() {
    let body = format!(
        r#"{}
[[sync]]
from = "videos/cats"
to = "univ/videos/cats"
[[sync]]
from = "configs"
to = "sys/server-configs"
"#,
        valid_roots()
    );
    let config = parse(&body).unwrap();
    let ids: Vec<_> = config.sync.iter().map(|sync| sync.name.as_str()).collect();
    assert_eq!(ids, ["sync-0001", "sync-0002"]);
}

#[test]
fn issue_34_examples_describe_only_root_based_client_config() {
    let client = include_str!("../../../examples/client.toml");
    let server = include_str!("../../../examples/server.toml");
    assert!(client.contains("[[root]]"));
    assert!(client.contains("from = \"videos/cats\""));
    assert!(client.contains("to = \"univ/videos/cats\""));
    assert!(!client.contains("[[postprocess.rules]]"));
    assert!(!client.contains("[postprocess.steps."));
    assert!(server.contains("[postprocess.steps.compress-image]"));
    assert!(!server.contains("[[sync]]"));
}

#[test]
fn issue_34_docs_keep_nickname_out_of_archive_paths() {
    for document in [
        include_str!("../../../README.md"),
        include_str!("../../../docs/config.md"),
        include_str!("../../../docs/protocol.md"),
        include_str!("../../../docs/design/import-semantics.md"),
    ] {
        assert!(!document.contains("/universe/synced/laptop"));
        assert!(!document.contains("univ/laptop/"));
    }
}
