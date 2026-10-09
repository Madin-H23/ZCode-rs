use super::*;

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn resolve_with(pairs: &[(&str, &str)]) -> Result<RuntimeCommand> {
    resolve(&vars(pairs), Path::new("/w"))
}

/// 不给命令时的组合：缺省、显式 `embedded`、空白值。
const WITHOUT_COMMAND: [&[(&str, &str)]; 3] = [
    &[],
    &[(RUNTIME_VAR, "embedded")],
    &[(RUNTIME_VAR, " "), (COMMAND_VAR, "")],
];

#[cfg(feature = "embedded")]
#[test]
fn in_process_is_the_default_and_the_explicit_choice() {
    for pairs in WITHOUT_COMMAND {
        let command = resolve_with(pairs).unwrap();
        assert_eq!(
            command,
            RuntimeCommand::InProcess(Purpose::Serve),
            "{pairs:?}"
        );
        assert!(command.supports_storage_startup());
        assert_eq!(command.cwd(), None);
        assert_eq!(command.program(), "zcode-runtime (embedded)");
        assert!(command.args().is_empty());
    }
}

/// “只有 Host”形态（规格 §3.3、§3.5）：不给命令就没有 runtime 可用，启动时报错并说明怎么指定。
#[cfg(not(feature = "embedded"))]
#[test]
fn a_host_only_build_needs_a_command() {
    for pairs in WITHOUT_COMMAND {
        let error = resolve_with(pairs).unwrap_err();
        assert_eq!(
            error.to_string(),
            "this zcode-host build does not include the in-process runtime; set \
             ZCODE_AGENT_SERVER_COMMAND to a zcode-cli-rust executable and \
             ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust",
            "{pairs:?}"
        );
    }
}

#[cfg(feature = "embedded")]
#[test]
fn the_implicit_discovery_chain_is_gone() {
    // Main 给 Node Host 准备的变量（以及开发态 bundle 的查找输入）不再选出子进程链路。
    let command = resolve_with(&[
        ("GLM_BINARY_PATH", "/bundle/zcode"),
        ("ZCODE_HOST_NODE_EXEC_PATH", "/Electron"),
        ("ZCODE_AGENT_BUNDLE_PATH", "/bundle/zcode.cjs"),
        ("ZCODE_DESKTOP_AGENT_BYTECODE", "1"),
    ])
    .unwrap();
    assert_eq!(command, RuntimeCommand::InProcess(Purpose::Serve));
}

#[test]
fn rust_cli_override_appends_cwd_and_surface() {
    let command = resolve_with(&[
        (COMMAND_VAR, "/bin/zcode-cli-rust"),
        (RUNTIME_VAR, "zcode-cli-rust"),
    ])
    .unwrap();
    assert_eq!(
        command,
        RuntimeCommand::Process(ProcessCommand {
            program: PathBuf::from("/bin/zcode-cli-rust"),
            args: [
                "app-server",
                "--stdio",
                "--cwd",
                "/w",
                "--surface",
                "desktop"
            ]
            .map(String::from)
            .to_vec(),
            cwd: Some(PathBuf::from("/w")),
            supports_storage_startup: true,
        })
    );
}

#[test]
fn a_command_without_a_runtime_is_a_protocol_double() {
    let command = resolve_with(&[
        (COMMAND_VAR, "/tmp/fake-cli"),
        ("ZCODE_AGENT_SERVER_CWD", "/elsewhere"),
    ])
    .unwrap();
    assert_eq!(
        command,
        RuntimeCommand::Process(ProcessCommand {
            program: PathBuf::from("/tmp/fake-cli"),
            args: ["app-server", "--stdio", "--surface", "desktop"]
                .map(String::from)
                .to_vec(),
            cwd: Some(PathBuf::from("/elsewhere")),
            supports_storage_startup: false,
        })
    );
    // 协议替身没有存储握手，不能用来准备会话库。
    assert_eq!(command.for_storage_preparation(Path::new("/w")), None);
}

#[test]
fn storage_preparation_reuses_the_command() {
    let command = RuntimeCommand::Process(ProcessCommand {
        program: PathBuf::from("/bin/zcode-cli-rust"),
        args: vec!["app-server".into(), "--stdio".into()],
        cwd: None,
        supports_storage_startup: true,
    });
    assert_eq!(
        command.for_storage_preparation(Path::new("/w")),
        Some(RuntimeCommand::Process(ProcessCommand {
            program: PathBuf::from("/bin/zcode-cli-rust"),
            args: ["app-server", "--stdio", "--prepare-storage"]
                .map(String::from)
                .to_vec(),
            cwd: Some(PathBuf::from("/w")),
            supports_storage_startup: true,
        }))
    );
    #[cfg(feature = "embedded")]
    assert_eq!(
        RuntimeCommand::InProcess(Purpose::Serve).for_storage_preparation(Path::new("/w")),
        Some(RuntimeCommand::InProcess(Purpose::PrepareStorage))
    );
}

#[test]
fn inconsistent_selections_are_rejected() {
    for pairs in [
        // 子进程链路必须给出命令，不能静默退回进程内。
        &[(RUNTIME_VAR, "zcode-cli-rust")][..],
        &[(RUNTIME_VAR, "node")][..],
        &[(COMMAND_VAR, "x"), (RUNTIME_VAR, "embedded")][..],
        &[(COMMAND_VAR, "x"), (RUNTIME_VAR, "node")][..],
        &[
            (COMMAND_VAR, "x"),
            (RUNTIME_VAR, "zcode-cli-rust"),
            (
                "ZCODE_AGENT_SERVER_ARGS_JSON",
                r#"["app-server","--cwd","/x"]"#,
            ),
        ][..],
        &[(COMMAND_VAR, "x"), ("ZCODE_AGENT_SERVER_ARGS_JSON", "{}")][..],
    ] {
        assert!(resolve_with(pairs).is_err(), "{pairs:?}");
    }
}

#[test]
fn surface_flag_is_replaced() {
    assert_eq!(
        apply_desktop_surface(vec![
            "a".into(),
            "--surface".into(),
            "terminal".into(),
            "--stdio".into()
        ]),
        ["a", "--stdio", "--surface", "desktop"]
    );
    assert_eq!(
        apply_desktop_surface(vec!["--surface".into(), "--stdio".into()]),
        ["--stdio", "--surface", "desktop"]
    );
}
