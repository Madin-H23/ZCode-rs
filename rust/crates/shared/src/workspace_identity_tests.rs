use super::*;

#[test]
fn workspace_refs_follow_node() {
    let local = Workspace::from_id("/repo").unwrap();
    assert_eq!(
        (local.key.as_str(), local.path.as_str()),
        ("/repo", "/repo")
    );
    assert_eq!(
        local.to_ref(),
        json!({"workspacePath":"/repo","workspaceKey":"/repo"})
    );
    let remote = Workspace::from_id("remote:ssh:h:22:me:/srv/app").unwrap();
    assert_eq!(remote.key, "remote:ssh:h:22:me:/srv/app");
    assert_eq!(remote.path, "/srv/app");
    assert_eq!(remote.to_ref()["workspaceIdentity"], remote.key.as_str());
    assert!(Workspace::from_id("remote:ssh:bad").is_err());
    assert!(Workspace::from_id("").is_err());
    let blank = json!({"workspacePath":"/repo","workspaceIdentity":"  "});
    assert_eq!(
        Workspace::from_ref(Some(&blank)).unwrap().unwrap().key,
        "/repo"
    );
    let named = json!({"workspacePath":"/repo","workspaceIdentity":" id-1 "});
    assert_eq!(
        Workspace::from_ref(Some(&named)).unwrap().unwrap().key,
        "id-1"
    );
    assert!(Workspace::from_ref(Some(&json!({"workspacePath":""}))).is_err());
    assert!(Workspace::from_ref(Some(&Value::Null)).unwrap().is_none());
    assert!(Workspace::from_ref(None).unwrap().is_none());
}

#[test]
fn parses_node_identity_formats() {
    let parse = |s| parse_remote(s).map(|r| (r.kind, r.path));
    assert_eq!(
        parse("remote:ssh:host:22:me:/srv/app"),
        Some(("ssh", "/srv/app"))
    );
    assert_eq!(parse("remote:ssh:host:22:me:/a:b"), Some(("ssh", "/a:b")));
    assert_eq!(parse("remote:wsl:Ubuntu:/home/x"), Some(("wsl", "/home/x")));
    assert_eq!(
        parse("remote:wsl:Ubuntu:me:/home/x"),
        Some(("wsl", "/home/x"))
    );
    assert_eq!(parse("remote:docker:box:/app"), Some(("docker", "/app")));
    for bad in [
        "/local/path",
        "remote:",
        "remote::x:/p",
        "remote:ftp:h:/p",
        "remote:ssh:host::me:/p",
        "remote:ssh:host:22:me:relative",
        "remote:wsl:Ubuntu:me",
        "remote:docker:box:",
    ] {
        assert_eq!(parse(bad), None, "{bad}");
    }
}
