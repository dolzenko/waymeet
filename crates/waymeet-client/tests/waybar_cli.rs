//! Exercise the actual CLI with the documented display settings and a test IPC server.
#![cfg(all(unix, feature = "ics"))]

use std::{path::PathBuf, time::Duration};

use chrono::{Local, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use waymeet_core::{EventTime, MeetingView, NormalizedEvent};
use waymeet_protocol::{Envelope, Request, Response};

async fn render(offset: Option<i64>) -> serde_json::Value {
    render_case(offset, false).await
}

async fn render_case(offset: Option<i64>, with_history: bool) -> serde_json::Value {
    // Optional overrides exercise an installed binary and only the display table
    // of a local configuration. Provider credentials are never resolved or copied.
    let config_path = std::env::var_os("WAYMEET_TEST_DISPLAY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml")
        });
    let source: toml::Value =
        toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    let mut config: toml::Value = toml::from_str(
        "[[ics]]\nname = 'test'\nurl = 'https://example.invalid/calendar.ics'\n[server]\nauto_start = false\n",
    ).unwrap();
    config
        .as_table_mut()
        .unwrap()
        .insert("display".into(), source["display"].clone());
    if with_history {
        let filters: toml::Value = toml::from_str("limit = 1").unwrap();
        config
            .as_table_mut()
            .unwrap()
            .insert("filters".into(), filters);
        config["display"].as_table_mut().unwrap().insert(
            "tooltip_include_past_today".into(),
            toml::Value::Boolean(true),
        );
    }
    let agenda_enabled = config["display"]
        .get("tooltip_include_past_today")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let socket_path = temp.path().join("test.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let now = Utc::now();
    let meetings: Vec<_> = offset
        .into_iter()
        .map(|minutes| {
            let start = now + chrono::Duration::minutes(minutes);
            let event = NormalizedEvent::new(
                "test",
                "Calendar regression",
                EventTime::from_utc(start),
                EventTime::from_utc(now + chrono::Duration::minutes(30)),
                "test",
            );
            MeetingView::from_event(&event, now)
        })
        .collect();
    let mut agenda = meetings.clone();
    if with_history {
        let start = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        let event = NormalizedEvent::new(
            "past",
            "Morning reminder",
            EventTime::from_utc(start),
            EventTime::from_utc(start),
            "test",
        );
        agenda.insert(0, MeetingView::from_event(&event, now));
    }
    let server = async {
        for include_past in [false, true]
            .into_iter()
            .take(if agenda_enabled { 2 } else { 1 })
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let length = stream.read_u32().await.unwrap();
            assert!(length <= waymeet_protocol::MAX_MESSAGE_SIZE);
            let mut body = vec![0; length as usize];
            stream.read_exact(&mut body).await.unwrap();
            let request: Envelope<Request> = serde_json::from_slice(&body).unwrap();
            let Request::GetMeetings { filter } = request.payload else {
                panic!("Expected meetings request")
            };
            assert_eq!(
                filter.as_ref().is_some_and(|f| f.include_past_today),
                include_past
            );
            if include_past {
                assert!(filter.unwrap().limit.is_none());
            }
            let response = Envelope::response(
                request.request_id,
                Response::meetings(if include_past {
                    agenda.clone()
                } else {
                    meetings.clone()
                }),
            );
            stream
                .write_all(&waymeet_protocol::encode_message(&response).unwrap())
                .await
                .unwrap();
        }
    };
    let binary = std::env::var_os("WAYMEET_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_waymeet").into());
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["--waybar", "--config"])
        .arg(config_path)
        .arg("--socket-path")
        .arg(socket_path)
        .kill_on_drop(true);
    let (_, output) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, command.output())
    })
    .await
    .expect("CLI/server exchange timed out");
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for field in ["text", "tooltip"] {
        let value = output[field].as_str().unwrap();
        assert!(
            !value.contains('{'),
            "unexpanded template in {field}: {value}"
        );
        if let Some(offset) = offset {
            assert!(value.contains("Calendar regression"));
            if field == "tooltip" {
                assert!(
                    value.chars().any(|c| c.is_ascii_digit()),
                    "missing tooltip time: {value}"
                );
            } else if offset > 0 {
                // The relative-time formatter deliberately uses an absolute time across midnight.
                let start = (now + chrono::Duration::minutes(offset)).with_timezone(&Local);
                assert!(
                    value.contains("In 15 minutes")
                        || value.contains(&start.format("%H:%M").to_string()),
                    "missing time: {value}"
                );
            } else {
                assert!(value.contains("to go"), "missing ongoing time: {value}");
            }
        }
    }
    output
}

#[tokio::test]
async fn upcoming_meeting_expands_display_and_tooltip_templates() {
    let output = render(Some(15)).await;
    assert_eq!(output["class"], "upcoming");
}

#[tokio::test]
async fn ongoing_meeting_shows_time_remaining() {
    let output = render(Some(-10)).await;
    assert_eq!(output["class"], "ongoing");
}

#[tokio::test]
async fn empty_calendar_has_no_unexpanded_template() {
    let output = render(None).await;
    assert!(!output["text"].as_str().unwrap().is_empty());
    assert_eq!(output["tooltip"], "No upcoming meetings");
}

#[tokio::test]
async fn agenda_does_not_replace_upcoming_chip_with_finished_event() {
    let output = render_case(Some(15), true).await;
    assert!(
        !output["text"]
            .as_str()
            .unwrap()
            .contains("Morning reminder")
    );
    assert!(
        output["tooltip"]
            .as_str()
            .unwrap()
            .contains("Morning reminder")
    );
    assert_eq!(output["class"], "upcoming");
}

#[tokio::test]
async fn finished_events_remain_in_tooltip_with_no_upcoming_chip() {
    let output = render_case(None, true).await;
    assert!(
        !output["text"]
            .as_str()
            .unwrap()
            .contains("Morning reminder")
    );
    assert!(
        output["tooltip"]
            .as_str()
            .unwrap()
            .contains("Morning reminder")
    );
    assert!(output["class"].is_null());
    let tooltip = output["tooltip"].as_str().unwrap();
    assert!(!tooltip.contains("Now"));
    assert!(tooltip.contains("00:00"));
}
