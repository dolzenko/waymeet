use super::*;
use crate::RawEventTime;
use chrono::{DateTime, Duration, Utc};
use waymeet_core::TimeWindow;

fn config() -> IcsConfig {
    IcsConfig::new("work", "https://example.invalid/private/calendar.ics").unwrap()
}
fn options(from: &str, days: i64) -> FetchOptions {
    let start = from.parse::<DateTime<Utc>>().unwrap();
    FetchOptions::new()
        .with_time_window(TimeWindow::from_duration(start, Duration::days(days)))
        .with_expand_recurring(true)
}
fn calendar(events: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{events}\r\nEND:VCALENDAR\r\n")
}
fn event(fields: &str) -> String {
    format!("BEGIN:VEVENT\n{fields}\nEND:VEVENT")
}
fn parsed(fields: &str, from: &str, days: i64) -> Vec<crate::RawEvent> {
    parse::events(&calendar(fields), &config(), &options(from, days)).unwrap()
}
fn datetime(time: &RawEventTime) -> String {
    match time {
        RawEventTime::DateTime(dt) => dt.to_rfc3339(),
        RawEventTime::Date(d) => d.to_string(),
    }
}

#[test]
fn windows_timezone_folded_text_links_and_reminders() {
    let fields = event(
        "UID:weekly\nDTSTART;TZID=Russian Standard Time:20261009T160000\nDTEND;TZID=Russian Standard Time:20261009T170000\nSUMMARY:Team\\, review\nDESCRIPTION:Join https://meet.google.com/abc-\n defg-hij\\nAgenda\\; notes\nBEGIN:VALARM\nACTION:DISPLAY\nTRIGGER:-PT15M\nEND:VALARM",
    );
    let events = parsed(&fields, "2026-10-09T00:00:00Z", 1);
    assert_eq!(events.len(), 1);
    assert_eq!(datetime(&events[0].start), "2026-10-09T13:00:00+00:00");
    assert_eq!(events[0].summary.as_deref(), Some("Team, review"));
    assert_eq!(
        events[0].description.as_deref(),
        Some("Join https://meet.google.com/abc-defg-hij\nAgenda; notes")
    );
    assert_eq!(events[0].reminder_minutes, Some(vec![15]));
    assert!(!crate::normalize_event(&events[0]).links.is_empty());
    assert_eq!(events[0].calendar_id, "work");
}

#[test]
fn recurrence_keeps_local_time_across_dst() {
    let fields = event(
        "UID:dst\nDTSTART;TZID=Europe/Berlin:20261024T090000\nDURATION:PT1H\nRRULE:FREQ=DAILY;COUNT=3",
    );
    let events = parsed(&fields, "2026-10-24T00:00:00Z", 3);
    assert_eq!(
        events
            .iter()
            .map(|e| datetime(&e.start))
            .collect::<Vec<_>>(),
        vec![
            "2026-10-24T07:00:00+00:00",
            "2026-10-25T08:00:00+00:00",
            "2026-10-26T08:00:00+00:00"
        ]
    );
    assert_ne!(events[0].id, events[1].id);
    assert!(events.iter().all(|e| e.is_recurring_instance));
}

#[test]
fn exdates_cancellation_moved_override_and_rdate() {
    let fields = [
        event("UID:series\nDTSTART:20261005T090000Z\nDURATION:PT1H\nRRULE:FREQ=DAILY;COUNT=5\nEXDATE:20261006T090000Z\nRDATE:20261010T090000Z\nSUMMARY:Original"),
        event("UID:series\nRECURRENCE-ID:20261007T090000Z\nSTATUS:CANCELLED"),
        event("UID:series\nRECURRENCE-ID:20261008T090000Z\nDTSTART:20261008T110000Z\nDTEND:20261008T123000Z\nSUMMARY:Moved"),
    ].join("\n");
    let events = parsed(&fields, "2026-10-05T00:00:00Z", 7);
    assert_eq!(events.len(), 4);
    assert_eq!(datetime(&events[1].start), "2026-10-08T11:00:00+00:00");
    assert_eq!(datetime(&events[1].end), "2026-10-08T12:30:00+00:00");
    assert_eq!(events[1].id, "series/20261008T090000Z");
    assert_eq!(events[1].summary.as_deref(), Some("Moved"));
}

#[test]
fn moved_occurrence_enters_window_from_outside() {
    let fields = [
        event("UID:series\nDTSTART:20260901T090000Z\nDURATION:PT1H\nRRULE:FREQ=DAILY;COUNT=2"),
        event(
            "UID:series\nRECURRENCE-ID:20260902T090000Z\nDTSTART:20261009T110000Z\nDURATION:PT1H",
        ),
    ]
    .join("\n");
    let events = parsed(&fields, "2026-10-09T00:00:00Z", 1);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, "series/20260902T090000Z");
}

#[test]
fn all_day_default_end_and_exclusive_window_boundaries() {
    let fields = [
        event("UID:all-day\nDTSTART;VALUE=DATE:20261009\nRRULE:FREQ=DAILY;COUNT=3"),
        event("UID:finished\nDTSTART:20261008T230000Z\nDTEND:20261009T000000Z"),
        event("UID:ongoing\nDTSTART:20261008T233000Z\nDTEND:20261009T003000Z"),
        event("UID:instant\nDTSTART:20261009T000000Z"),
        event("UID:outside\nDTSTART:20261010T000000Z"),
    ]
    .join("\n");
    let events = parsed(&fields, "2026-10-09T00:00:00Z", 1);
    assert_eq!(events.len(), 3);
    let all_day = events
        .iter()
        .find(|e| e.id.starts_with("all-day/"))
        .unwrap();
    assert_eq!(datetime(&all_day.start), "2026-10-09");
    assert_eq!(datetime(&all_day.end), "2026-10-10");
}

#[test]
fn aliases_and_floating_timezone() {
    let mut cfg = config();
    cfg.timezone = "Europe/Moscow".into();
    cfg.timezone_aliases
        .insert("Custom Zone".into(), "Asia/Barnaul".into());
    let fields = [
        event("UID:a\nDTSTART;TZID=Custom Zone:20261009T160000"),
        event("UID:b\nDTSTART:20261009T160000"),
    ]
    .join("\n");
    let events = parse::events(
        &calendar(&fields),
        &cfg,
        &options("2026-10-09T00:00:00Z", 1),
    )
    .unwrap();
    assert_eq!(datetime(&events[0].start), "2026-10-09T09:00:00+00:00");
    assert_eq!(datetime(&events[1].start), "2026-10-09T13:00:00+00:00");
}

#[test]
fn newest_revision_wins_and_cancelled_series_disappears() {
    let fields = [
        event("UID:a\nSEQUENCE:1\nDTSTART:20261009T100000Z\nSUMMARY:Old"),
        event("UID:a\nSEQUENCE:2\nDTSTART:20261009T110000Z\nSUMMARY:New"),
        event("UID:b\nDTSTART:20261009T110000Z\nRRULE:FREQ=DAILY\nSTATUS:CANCELLED"),
    ]
    .join("\n");
    let events = parsed(&fields, "2026-10-09T00:00:00Z", 1);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].summary.as_deref(), Some("New"));
}

#[test]
fn malformed_feeds_fail_instead_of_erasing_cached_events() {
    for body in [
        "<html>Sign in</html>".into(),
        "BEGIN:VCALENDAR\nVERSION:2.0".into(),
        calendar(&event("UID:a\nDTSTART;TZID=Unknown/Zone:20261009T100000")),
        calendar(&event("UID:a\nDTSTART:20261009T100000Z\nRRULE:FREQ=NOPE")),
        calendar(&event(
            "UID:a\nDTSTART:20261009T100000Z\nDTEND:20261009T090000Z",
        )),
        calendar(&event(
            "UID:a\nDTSTART:20261009T100000Z\nDURATION:P999999999999999999D",
        )),
        calendar(&event(
            "UID:a\nRECURRENCE-ID;RANGE=THISANDFUTURE:20261009T100000Z\nDTSTART:20261009T110000Z",
        )),
    ] {
        assert!(parse::events(&body, &config(), &options("2026-10-09T00:00:00Z", 1)).is_err());
    }
    assert!(
        parse::events(
            &calendar(""),
            &config(),
            &options("2026-10-09T00:00:00Z", 1)
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn webcal_normalisation_validation_and_result_limit() {
    assert_eq!(
        IcsConfig::new("work", "webcal://example.org/a.ics")
            .unwrap()
            .url
            .scheme(),
        "https"
    );
    for url in [
        "file:///tmp/a.ics",
        "https://user:secret@example.org/a.ics",
        "not a url",
    ] {
        let error = IcsConfig::new("work", url).err().unwrap().to_string();
        assert!(!error.contains("secret"));
    }
    assert!(IcsConfig::new("bad/name", "https://example.org").is_err());
    let fields = event("UID:a\nDTSTART:20261009T100000Z\nRRULE:FREQ=DAILY;COUNT=3");
    let opts = options("2026-10-09T00:00:00Z", 4).with_max_results(1);
    assert_eq!(
        parse::events(&calendar(&fields), &config(), &opts)
            .unwrap()
            .len(),
        1
    );
}

/// A real HTTP exchange verifies conditional requests and cache transitions.
#[tokio::test]
async fn http_cache_reexpands_304_and_rejects_bad_replacement() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = calendar(&event(
        "UID:a\nDTSTART:20261009T100000Z\nRRULE:FREQ=DAILY;COUNT=3",
    ));
    let server = std::thread::spawn(move || {
        let responses = [
            format!("HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()),
            "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nETag: \"bad\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n<html".into(),
            "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 42\r\nConnection: close\r\n\r\nBEGIN:VCALENDAR\nVERSION:2.0\nEND:VCALENDAR\n".into(),
        ];
        for (i, response) in responses.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /private/feed.ics"));
            assert!(request.contains("accept: text/calendar,text/plain,*/*"));
            assert!(request.contains("user-agent: test-agent"));
            if i > 0 {
                assert!(request.contains("if-none-match: \"v1\""));
            }
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let mut cfg = IcsConfig::new("work", &format!("http://{address}/private/feed.ics")).unwrap();
    cfg.user_agent = "test-agent".into();
    let provider = IcsProvider::new(cfg).unwrap();
    let first = provider
        .fetch_events(options("2026-10-09T00:00:00Z", 1))
        .await
        .unwrap();
    let second = provider
        .fetch_events(options("2026-10-10T00:00:00Z", 1))
        .await
        .unwrap();
    assert_ne!(first.events[0].id, second.events[0].id);
    assert!(!second.not_modified);
    assert!(
        provider
            .fetch_events(options("2026-10-10T00:00:00Z", 1))
            .await
            .is_err()
    );
    assert_eq!(
        provider
            .fetch_events(options("2026-10-10T00:00:00Z", 1))
            .await
            .unwrap()
            .events,
        second.events
    );
    assert!(
        provider
            .fetch_events(options("2026-10-10T00:00:00Z", 1))
            .await
            .unwrap()
            .events
            .is_empty()
    );
    server.join().unwrap();
    assert!(provider.is_authenticated());
    assert_eq!(provider.name(), "ics:work");
}

#[tokio::test]
async fn errors_do_not_expose_access_bearing_url() {
    let provider = IcsProvider::new(
        IcsConfig::new("work", "http://127.0.0.1:1/super-secret-token/calendar.ics").unwrap(),
    )
    .unwrap();
    let error = provider
        .fetch_events(FetchOptions::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("super-secret-token"));
    assert!(!error.contains("127.0.0.1"));
}

#[test]
fn unbounded_dense_recurrence_reports_limit() {
    let fields = event("UID:dense\nDTSTART:20261009T000000Z\nRRULE:FREQ=SECONDLY");
    let result = parse::events(
        &calendar(&fields),
        &config(),
        &options("2026-10-09T00:00:00Z", 1),
    );
    assert!(result.unwrap_err().to_string().contains("limit"));
}

#[test]
fn monthly_rule_until_and_multiple_exdates() {
    let fields = event(
        "UID:monthly\nDTSTART:20261001T090000Z\nDURATION:PT30M\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;UNTIL=20270201T090000Z\nEXDATE:20261102T090000Z,20261201T090000Z",
    );
    let events = parsed(&fields, "2026-10-01T00:00:00Z", 160);
    assert_eq!(
        events
            .iter()
            .map(|e| datetime(&e.start))
            .collect::<Vec<_>>(),
        vec![
            "2026-10-01T09:00:00+00:00",
            "2027-01-01T09:00:00+00:00",
            "2027-02-01T09:00:00+00:00"
        ]
    );
}

#[test]
fn all_day_recurrence_until_date_is_inclusive() {
    let fields = event(
        "UID:all-day-until\nDTSTART;VALUE=DATE:20261001\nDTEND;VALUE=DATE:20261002\nRRULE:FREQ=DAILY;UNTIL=20261003\nSUMMARY:All day",
    );
    let events = parsed(&fields, "2026-10-01T00:00:00Z", 5);
    assert_eq!(
        events
            .iter()
            .map(|e| datetime(&e.start))
            .collect::<Vec<_>>(),
        ["2026-10-01", "2026-10-02", "2026-10-03"]
    );
    assert_eq!(
        events.iter().map(|e| datetime(&e.end)).collect::<Vec<_>>(),
        ["2026-10-02", "2026-10-03", "2026-10-04"]
    );
    assert!(parsed(&fields, "2026-10-04T00:00:00Z", 5).is_empty());
}
