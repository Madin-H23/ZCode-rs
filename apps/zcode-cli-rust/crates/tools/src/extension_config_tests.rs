use super::*;

#[test]
fn resolve_expands_tilde_and_folds_dots() {
    let (base, home) = (Path::new("/work/project"), Path::new("/home/u"));
    for (input, expected) in [
        ("~", "/home/u"),
        ("~/skills/../agents", "/home/u/agents"),
        ("./skills", "/work/project/skills"),
        ("../shared", "/work/shared"),
        ("/abs/./x", "/abs/x"),
        ("~other/x", "/work/project/~other/x"),
    ] {
        assert_eq!(
            resolve(base, input, home),
            PathBuf::from(expected),
            "{input}"
        );
    }
}
