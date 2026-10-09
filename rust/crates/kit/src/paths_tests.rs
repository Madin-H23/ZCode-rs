use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::*;

fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
    }
}

#[test]
fn home_prefers_home_then_userprofile() {
    assert_eq!(
        home_dir_with(env(&[("HOME", "/h"), ("USERPROFILE", "/u")])).unwrap(),
        PathBuf::from("/h")
    );
    assert_eq!(
        home_dir_with(env(&[("HOME", ""), ("USERPROFILE", "/u")])).unwrap(),
        PathBuf::from("/u")
    );
}

/// B4：`HOME` 为空且没有 `USERPROFILE` 时，旧实现返回空路径，数据会写到当前工作目录下。
#[test]
fn empty_home_never_resolves_to_an_empty_path() {
    for lookup in [
        env(&[("HOME", "")]),
        env(&[("HOME", ""), ("USERPROFILE", "")]),
    ] {
        let resolved = home_dir_with(lookup);
        #[cfg(unix)]
        {
            let home = resolved.unwrap();
            assert!(
                home.is_absolute(),
                "falls back to the user database, got {home:?}"
            );
            assert_eq!(home_dir_with(|_| None).unwrap(), home);
        }
        #[cfg(not(unix))]
        assert_eq!(resolved.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }
}

#[test]
fn expands_tilde_against_home() {
    let home = Path::new("/home/u");
    let cases: &[(&str, PathBuf)] = &[
        ("~", home.to_path_buf()),
        ("~/x/y", home.join("x/y")),
        ("~//x", home.join("x")),
        ("~user/x", PathBuf::from("~user/x")),
        ("x/~", PathBuf::from("x/~")),
        ("/abs", PathBuf::from("/abs")),
        ("rel", PathBuf::from("rel")),
        ("", PathBuf::new()),
    ];
    for (input, expected) in cases {
        assert_eq!(&expand_home(input, home), expected, "input {input:?}");
    }
    let windows_style = expand_home("~\\x", home);
    if cfg!(windows) {
        assert_eq!(windows_style, home.join("x"));
    } else {
        assert_eq!(windows_style, PathBuf::from("~\\x"));
    }
}
