use super::*;

fn subscriber(tx: SyncSender<Message>, dropped: Arc<AtomicU64>) -> JsonLines {
    JsonLines {
        level: Level::INFO,
        tx,
        dropped,
    }
}

fn next_line(rx: &Receiver<Message>) -> Value {
    let Ok(Message::Line(line)) = rx.recv() else {
        panic!("expected a log line")
    };
    serde_json::from_str(&line).unwrap()
}

#[test]
fn lines_follow_node_fields_and_redact_credentials() {
    let (tx, rx) = sync_channel(8);
    tracing::subscriber::with_default(subscriber(tx, Arc::default()), || {
        tracing::info!(
            target: "zcode::test",
            event = "session.started",
            session_id = "s1",
            api_key = "secret-value",
            count = 3,
            "Session started"
        );
        tracing::debug!(target: "zcode::test", "hidden at info level");
        tracing::info!(target: "hyper::proto", "dependency noise");
    });
    let line = next_line(&rx);
    assert_eq!(line["level"], "info");
    assert_eq!(line["event"], "session.started");
    assert_eq!(line["sessionId"], "s1");
    assert_eq!(line["message"], "Session started");
    assert_eq!(line["context"]["api_key"], "[Redacted]");
    assert_eq!(line["context"]["count"], 3);
    assert!(
        rx.try_recv().is_err(),
        "debug and dependency events are filtered"
    );
}

#[test]
fn secret_values_are_redacted_whatever_the_field_name() {
    let (tx, rx) = sync_channel(8);
    let failure = std::io::Error::other("401 for Bearer abcdefghijklmnopqrstuv");
    tracing::subscriber::with_default(subscriber(tx, Arc::default()), || {
        tracing::warn!(
            target: "zcode::test",
            detail = "retry with sk-abcdefghijklmnopqrstuvwx",
            error = &failure as &(dyn std::error::Error + 'static),
            "Provider rejected Authorization: Bearer abcdefghijklmnopqrstuv"
        );
    });
    let line = next_line(&rx);
    assert_eq!(
        (&line["message"], &line["context"]["detail"], &line["error"]),
        (
            &json!("Provider rejected Authorization: Bearer [REDACTED]"),
            &json!("retry with [REDACTED]"),
            &json!({"message": "401 for Bearer [REDACTED]"}),
        )
    );
}

#[test]
fn full_queue_drops_instead_of_blocking() {
    let (tx, _rx) = sync_channel(1);
    let dropped = Arc::new(AtomicU64::new(0));
    tracing::subscriber::with_default(subscriber(tx, dropped.clone()), || {
        for _ in 0..3 {
            tracing::info!(target: "zcode::test", "line");
        }
    });
    assert_eq!(dropped.load(Ordering::Relaxed), 2);
}

fn writer(directory: &std::path::Path, dropped: &Arc<AtomicU64>) -> SyncSender<Message> {
    let (tx, rx) = sync_channel(8);
    let writer = Writer {
        directory: directory.to_path_buf(),
        console: false,
        current: None,
        dropped: dropped.clone(),
    };
    std::thread::spawn(move || writer.run(rx));
    tx
}

fn flush(tx: &SyncSender<Message>) {
    let (ack, done) = sync_channel(1);
    tx.send(Message::Flush(ack)).unwrap();
    done.recv().unwrap();
}

fn written(directory: &std::path::Path) -> (std::path::PathBuf, Vec<Value>) {
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let file = files.remove(0);
    let lines = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (file, lines)
}

#[test]
fn shutdown_flush_reports_dropped_lines_once() {
    let root = tempfile::tempdir().unwrap();
    let dropped = Arc::new(AtomicU64::new(0));
    let tx = writer(root.path(), &dropped);
    tx.send(Message::Line(r#"{"message":"kept"}"#.into()))
        .unwrap();
    flush(&tx);
    assert_eq!(written(root.path()).1, vec![json!({"message": "kept"})]);

    dropped.store(3, Ordering::Relaxed);
    flush(&tx);
    let (_, lines) = written(root.path());
    let report = &lines[1];
    assert_eq!(lines.len(), 2);
    assert_eq!(
        (
            &report["level"],
            &report["module"],
            &report["event"],
            &report["context"]
        ),
        (
            &json!("warn"),
            &json!("zcode::logging"),
            &json!("logging.lines_dropped"),
            &json!({"dropped": 3}),
        )
    );
}

#[cfg(unix)]
#[test]
fn log_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tx = writer(root.path(), &Arc::default());
    tx.send(Message::Line("{}".into())).unwrap();
    flush(&tx);
    let (file, _) = written(root.path());
    let mode = std::fs::metadata(file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
