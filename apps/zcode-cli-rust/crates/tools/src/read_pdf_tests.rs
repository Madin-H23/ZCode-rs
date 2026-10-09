//! Renderer availability across runtimes (spec rust-u1-runtime-library L6).
use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn renderer_availability_is_cached_per_search_path() {
    let found = tempfile::tempdir().unwrap();
    let script = found.path().join("pdftoppm");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let missing = tempfile::tempdir().unwrap();
    let env = |dir: &Path| vec![("PATH".to_owned(), dir.to_string_lossy().into_owned())];
    let cancel = CancellationToken::new();
    renderer(&env(found.path()), &cancel).await.unwrap();
    // 另一个 runtime 的 PATH 里没有 pdftoppm：不能沿用前一个 runtime 的探测结果。
    let error = renderer(&env(missing.path()), &cancel).await.unwrap_err();
    let expected = ToolError::Handler {
        code: code::CONFIGURATION,
        message: "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.".into(),
    };
    assert_eq!(error.downcast_ref::<ToolError>(), Some(&expected));
}
