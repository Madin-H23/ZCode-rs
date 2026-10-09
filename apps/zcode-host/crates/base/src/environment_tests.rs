use super::*;

#[test]
fn environment_prefers_explicit_overrides() {
    let env = HostEnvironment::capture([
        ("HOME".to_owned(), "/home/u".to_owned()),
        ("ZCODE_DESKTOP_HOME_DIR".to_owned(), "/tmp/desk".to_owned()),
    ])
    .unwrap();
    assert_eq!(env.home_dir, PathBuf::from("/tmp/desk"));
    assert_eq!(env.data_base_dir, PathBuf::from("/home/u"));
    assert_eq!(env.app_config_dir(), PathBuf::from("/home/u/.zcode/v2"));
}

/// B4：没有可用的 HOME / USERPROFILE 时不再得到空路径（数据会落到 Host 的 cwd 下）。
#[cfg(unix)]
#[test]
fn environment_without_home_uses_the_user_database() {
    let env = HostEnvironment::capture([("HOME".to_owned(), "  ".to_owned())]).unwrap();
    assert!(env.user_home.is_absolute(), "{:?}", env.user_home);
    assert_eq!(env.home_dir, env.user_home);
    assert_eq!(env.data_base_dir, env.user_home);
}
