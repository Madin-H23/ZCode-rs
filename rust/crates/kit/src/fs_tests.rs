use std::path::Path;
use std::time::Duration;

use super::*;

const NO_RETRY: &[Duration] = &[];

fn options(mode: FileMode, temp_name: TempName, parent: ParentDir) -> WriteOptions {
    WriteOptions {
        mode,
        temp_name,
        parent,
        rename_retry: NO_RETRY,
    }
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn replaces_the_target_and_leaves_no_temp_file() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("setting.json");
    std::fs::write(&target, "old").unwrap();
    for style in [TempName::Hidden, TempName::Visible] {
        let content = format!("{{\"style\":\"{style:?}\"}}");
        atomic_write(
            &target,
            content.as_bytes(),
            options(FileMode::Default, style, ParentDir::Existing),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
        assert_eq!(entries(directory.path()), vec!["setting.json".to_owned()]);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn private_files_are_exactly_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("credentials.json");
    std::fs::write(&target, "{}").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    atomic_write(
        &target,
        b"{\"k\":1}",
        options(FileMode::Private, TempName::Hidden, ParentDir::Existing),
    )
    .await
    .unwrap();
    let mode = std::fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(std::fs::read(&target).unwrap(), b"{\"k\":1}");
}

#[tokio::test]
async fn creates_missing_parents_only_when_asked() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("a/b/config.json");
    let missing = atomic_write(
        &target,
        b"x",
        options(FileMode::Default, TempName::Hidden, ParentDir::Existing),
    )
    .await;
    assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
    assert!(entries(directory.path()).is_empty());
    atomic_write(
        &target,
        b"x",
        options(FileMode::Default, TempName::Hidden, ParentDir::Create),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"x");
}

#[tokio::test]
async fn failed_rename_removes_the_temp_file_and_keeps_the_target() {
    let directory = tempfile::tempdir().unwrap();
    // 目标是一个非空目录：rename 失败且不可重试。
    let target = directory.path().join("occupied");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep"), "kept").unwrap();
    for style in [TempName::Hidden, TempName::Visible] {
        let result = atomic_write(
            &target,
            b"new",
            options(FileMode::Private, style, ParentDir::Existing),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(entries(directory.path()), vec!["occupied".to_owned()]);
        assert_eq!(
            std::fs::read_to_string(target.join("keep")).unwrap(),
            "kept"
        );
    }
}

#[test]
fn temp_names_follow_the_node_conventions() {
    let pid = std::process::id().to_string();
    let hidden = temp_name(OsStr::new("credentials.json"), TempName::Hidden);
    let visible = temp_name(OsStr::new("setting.json"), TempName::Visible);
    let (hidden, visible) = (hidden.to_str().unwrap(), visible.to_str().unwrap());
    for (name, prefix) in [(hidden, ".credentials.json."), (visible, "setting.json.")] {
        let rest = name.strip_prefix(prefix).unwrap();
        let parts: Vec<&str> = rest.strip_suffix(".tmp").unwrap().split('.').collect();
        assert_eq!(parts.len(), 3, "{name}");
        assert_eq!(parts[0], pid);
        assert!(parts[1].parse::<u128>().is_ok(), "{name}");
        assert_eq!(parts[2].len(), 16, "{name}");
    }
    assert_ne!(
        temp_name(OsStr::new("a"), TempName::Hidden),
        temp_name(OsStr::new("a"), TempName::Hidden)
    );
}

#[test]
fn only_busy_or_denied_renames_are_retried() {
    for (kind, expected) in [
        (ErrorKind::PermissionDenied, true),
        (ErrorKind::ResourceBusy, true),
        (ErrorKind::NotFound, false),
        (ErrorKind::IsADirectory, false),
    ] {
        assert_eq!(retryable(&io::Error::from(kind)), expected, "{kind:?}");
    }
}

#[tokio::test]
async fn rejects_targets_without_a_file_name() {
    let result = atomic_write(
        Path::new("/"),
        b"x",
        options(FileMode::Default, TempName::Hidden, ParentDir::Existing),
    )
    .await;
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}
