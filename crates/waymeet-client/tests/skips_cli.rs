//! Real CLI/IPC regression: skip, join the next event, undo and empty-state errors.
#![cfg(all(unix, feature = "ics"))]

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use waymeet_core::{EventLink, EventTime, LinkKind, MeetingView, NormalizedEvent};
use waymeet_protocol::{Envelope, Request};
use waymeet_server::{RequestHandler, new_shared_state};

#[tokio::test]
async fn skip_and_undo_drive_display_tooltip_and_join_through_real_ipc() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
[[ics]]
name = "test"
url = "https://example.invalid/calendar.ics"
[server]
auto_start = false
[display]
open_with = "echo"
[filters]
include_calendars = ["other"]
limit = 1
"#,
    )
    .unwrap();
    let socket = temp.path().join("test.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state = new_shared_state();
    let now = Utc::now();
    let meetings: Vec<_> = [
        ("first", "work", 10),
        ("next", "work", 20),
        ("personal", "personal", 30),
    ]
    .into_iter()
    .map(|(id, calendar, offset)| {
        let event = NormalizedEvent::new(
            id,
            id,
            EventTime::from_utc(now + chrono::Duration::minutes(offset)),
            EventTime::from_utc(now + chrono::Duration::hours(1)),
            calendar,
        );
        let mut meeting = MeetingView::from_event_with_provider(&event, "ics:test", now);
        meeting.primary_link = Some(EventLink::new(
            LinkKind::Other,
            format!("https://meet.example/{id}"),
        ));
        meeting
    })
    .collect();
    {
        let mut state = state.write().await;
        state
            .load_dismissed_events(temp.path().join("dismissed-events.json"))
            .unwrap();
        state.set_meetings(meetings);
    }
    let handler = Arc::new(RequestHandler::new(state));
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let len = stream.read_u32().await.unwrap();
            assert!(len <= waymeet_protocol::MAX_MESSAGE_SIZE);
            let mut body = vec![0; len as usize];
            stream.read_exact(&mut body).await.unwrap();
            let request: Envelope<Request> = serde_json::from_slice(&body).unwrap();
            let response =
                Envelope::response(request.request_id, handler.handle(&request.payload).await);
            stream
                .write_all(&waymeet_protocol::encode_message(&response).unwrap())
                .await
                .unwrap();
        }
    });
    let binary: PathBuf = std::env::var_os("WAYMEET_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_waymeet").into())
        .into();
    let run = |args: &[&str]| {
        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("--config")
            .arg(&config)
            .arg("--socket-path")
            .arg(&socket)
            .args(["--calendar", "work"])
            .args(args)
            .kill_on_drop(true);
        async move {
            tokio::time::timeout(Duration::from_secs(10), command.output())
                .await
                .unwrap()
                .unwrap()
        }
    };
    let initial = run(&["--waybar"]).await;
    assert!(initial.status.success());
    let initial: serde_json::Value = serde_json::from_slice(&initial.stdout).unwrap();
    assert!(initial["text"].as_str().unwrap().contains("first"));
    assert!(run(&["--skip"]).await.status.success());
    let advanced = run(&["--waybar"]).await;
    let advanced: serde_json::Value = serde_json::from_slice(&advanced.stdout).unwrap();
    assert!(advanced["text"].as_str().unwrap().contains("next"));
    assert!(!advanced["tooltip"].as_str().unwrap().contains("first"));
    let join = run(&["--open-meet-url"]).await;
    assert!(join.status.success());
    assert_eq!(
        String::from_utf8(join.stdout).unwrap().trim(),
        "https://meet.example/next"
    );
    assert!(run(&["--skip"]).await.status.success());
    let empty = run(&["--skip"]).await;
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("No meeting to skip."));
    assert!(run(&["--undo-skip"]).await.status.success());
    assert!(run(&["--undo-skip"]).await.status.success());
    assert!(!run(&["--undo-skip"]).await.status.success());
    let restored = run(&["--json"]).await;
    let restored: serde_json::Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(restored["next_meeting"]["title"], "first");
    assert!(!run(&["--skip", "--undo-skip"]).await.status.success());
    server.abort();
}
