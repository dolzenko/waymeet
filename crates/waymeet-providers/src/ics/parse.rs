//! Recurrence expansion uses local wall-clock times, then converts instances to UTC.
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use ical::parser::ical::component::IcalEvent;
use ical::property::Property;
use rrule::{RRule, RRuleSet, Tz, Unvalidated};
use windows_timezones::WindowsTimezone;

use super::IcsConfig;
use crate::{FetchOptions, ProviderError, ProviderResult, RawEvent, RawEventTime};

fn invalid(message: &str) -> ProviderError {
    ProviderError::invalid_response(message)
}

pub(super) fn timezone(name: &str, aliases: &BTreeMap<String, String>) -> ProviderResult<Tz> {
    let name = aliases.get(name).map(String::as_str).unwrap_or(name);
    let iana = name
        .parse::<WindowsTimezone>()
        .map(|tz| tz.tzdb_id())
        .unwrap_or(name);
    iana.parse::<chrono_tz::Tz>().map(Tz::from).map_err(|_| {
        invalid("Unknown ICS timezone; configure timezone_aliases with its IANA equivalent")
    })
}

fn prop<'a>(props: &'a [Property], name: &str) -> Option<&'a Property> {
    props.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}
fn value<'a>(props: &'a [Property], name: &str) -> Option<&'a str> {
    prop(props, name).and_then(|p| p.value.as_deref())
}
fn param<'a>(p: &'a Property, name: &str) -> Option<&'a str> {
    p.params
        .as_ref()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?
        .1
        .first()
        .map(|s| s.trim_matches('"'))
}

#[derive(Clone, Copy)]
struct Stamp {
    dt: DateTime<Tz>,
    all_day: bool,
}
impl Stamp {
    fn raw(self) -> RawEventTime {
        if self.all_day {
            RawEventTime::from_date(self.dt.date_naive())
        } else {
            RawEventTime::from_datetime(self.dt.with_timezone(&Utc))
        }
    }
    fn key(self) -> String {
        if self.all_day {
            self.dt.format("%Y%m%d").to_string()
        } else {
            self.dt
                .with_timezone(&Utc)
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        }
    }
}

fn stamp(p: &Property, default_tz: Tz, config: &IcsConfig) -> ProviderResult<Stamp> {
    let text = p
        .value
        .as_deref()
        .ok_or_else(|| invalid("Missing ICS date value"))?;
    if param(p, "VALUE") == Some("PERIOD") {
        return Err(invalid("ICS RDATE PERIOD is not supported"));
    }
    if text.len() == 8 || param(p, "VALUE") == Some("DATE") {
        let date =
            NaiveDate::parse_from_str(text, "%Y%m%d").map_err(|_| invalid("Invalid ICS date"))?;
        return Ok(Stamp {
            dt: Tz::UTC.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()),
            all_day: true,
        });
    }
    let tz = if text.ends_with('Z') {
        Tz::UTC
    } else if let Some(id) = param(p, "TZID") {
        timezone(id, &config.timezone_aliases)?
    } else {
        default_tz
    };
    let local = NaiveDateTime::parse_from_str(text.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
        .map_err(|_| invalid("Invalid ICS datetime"))?;
    let dt = tz
        .from_local_datetime(&local)
        .earliest()
        .ok_or_else(|| invalid("ICS datetime falls in a timezone gap"))?;
    Ok(Stamp { dt, all_day: false })
}

fn start(event: &IcalEvent, tz: Tz, config: &IcsConfig) -> ProviderResult<Stamp> {
    stamp(
        prop(&event.properties, "DTSTART").ok_or_else(|| invalid("ICS event has no DTSTART"))?,
        tz,
        config,
    )
}

fn duration(event: &IcalEvent, start: Stamp, config: &IcsConfig) -> ProviderResult<Duration> {
    let duration = if let Some(end) = prop(&event.properties, "DTEND") {
        let end = stamp(end, start.dt.timezone(), config)?;
        if end.all_day != start.all_day {
            return Err(invalid("ICS DTSTART and DTEND types differ"));
        }
        end.dt.signed_duration_since(start.dt)
    } else if let Some(text) = value(&event.properties, "DURATION") {
        parse_duration(text)?
    } else if start.all_day {
        Duration::days(1)
    } else {
        Duration::zero()
    };
    if duration < Duration::zero() {
        return Err(invalid("ICS event has negative duration"));
    }
    Ok(duration)
}

/// RFC 5545 durations contain weeks/days/hours/minutes/seconds, never months.
fn parse_duration(text: &str) -> ProviderResult<Duration> {
    let (sign, text) = if let Some(s) = text.strip_prefix('-') {
        (-1, s)
    } else {
        (1, text.strip_prefix('+').unwrap_or(text))
    };
    let text = text
        .strip_prefix('P')
        .ok_or_else(|| invalid("Invalid ICS duration"))?;
    let mut seconds = 0_i64;
    let mut digits = String::new();
    let mut time = false;
    let mut units = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        if c == 'T' && !time && digits.is_empty() {
            time = true;
            continue;
        }
        let multiplier = match (c, time) {
            ('W', false) => 604800,
            ('D', false) => 86400,
            ('H', true) => 3600,
            ('M', true) => 60,
            ('S', true) => 1,
            _ => return Err(invalid("Invalid ICS duration unit")),
        };
        let number: i64 = digits
            .parse()
            .map_err(|_| invalid("Invalid ICS duration number"))?;
        seconds = number
            .checked_mul(multiplier)
            .and_then(|n| seconds.checked_add(n))
            .ok_or_else(|| invalid("ICS duration is too large"))?;
        digits.clear();
        units += 1;
    }
    if !digits.is_empty() || units == 0 {
        return Err(invalid("Invalid ICS duration"));
    }
    Duration::try_seconds(seconds * sign).ok_or_else(|| invalid("ICS duration is too large"))
}

fn unescape(text: &str) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(c @ ('\\' | ',' | ';')) => out.push(c),
            Some(c) => {
                out.push('\\');
                out.push(c);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn cancelled(event: &IcalEvent) -> bool {
    value(&event.properties, "STATUS").is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED"))
}

fn raw_event(
    event: &IcalEvent,
    uid: &str,
    begin: Stamp,
    length: Duration,
    original: Option<Stamp>,
    config: &IcsConfig,
) -> ProviderResult<RawEvent> {
    let end = begin
        .dt
        .checked_add_signed(length)
        .ok_or_else(|| invalid("ICS end time is out of range"))?;
    let id = original
        .map(|s| format!("{uid}/{}", s.key()))
        .unwrap_or_else(|| uid.to_owned());
    let mut raw = RawEvent::new(
        id,
        begin.raw(),
        Stamp { dt: end, ..begin }.raw(),
        &config.name,
    );
    raw.summary = value(&event.properties, "SUMMARY").map(unescape);
    raw.description = value(&event.properties, "DESCRIPTION").map(unescape);
    raw.location = value(&event.properties, "LOCATION").map(unescape);
    raw.html_link = value(&event.properties, "URL").map(str::to_owned);
    raw.timezone = Some(begin.dt.timezone().name().into());
    raw.status = value(&event.properties, "STATUS").map(|s| s.to_ascii_lowercase());
    if let Some(original) = original {
        raw.is_recurring_instance = true;
        raw.recurring_event_id = Some(uid.to_owned());
        raw.original_start = Some(original.raw());
    }
    let mut reminders = Vec::new();
    for alarm in &event.alarms {
        if !matches!(
            value(&alarm.properties, "ACTION"),
            Some("DISPLAY" | "AUDIO")
        ) {
            continue;
        }
        if let Some(trigger) = prop(&alarm.properties, "TRIGGER") {
            if param(trigger, "VALUE") == Some("DATE-TIME")
                || param(trigger, "RELATED") == Some("END")
            {
                continue;
            }
            let offset = parse_duration(trigger.value.as_deref().unwrap_or_default())?;
            if offset <= Duration::zero() {
                reminders.push(
                    u32::try_from(-offset.num_minutes())
                        .map_err(|_| invalid("ICS reminder is too large"))?,
                );
            }
        }
    }
    reminders.sort_unstable();
    reminders.dedup();
    if !reminders.is_empty() {
        raw.reminder_minutes = Some(reminders);
    }
    Ok(raw)
}

pub(super) fn events(
    content: &str,
    config: &IcsConfig,
    options: &FetchOptions,
) -> ProviderResult<Vec<RawEvent>> {
    let content = content.trim_start_matches('\u{feff}').trim();
    if !content.starts_with("BEGIN:VCALENDAR") || !content.ends_with("END:VCALENDAR") {
        return Err(invalid(
            "Expected a complete ICS calendar, not an HTML login page or truncated response",
        ));
    }
    let mut calendars = ical::IcalParser::new(Cursor::new(content));
    let calendar = calendars
        .next()
        .ok_or_else(|| invalid("Empty ICS response"))?
        .map_err(|_| invalid("Malformed ICS calendar"))?;
    if calendars.next().is_some() {
        return Err(invalid("Expected one VCALENDAR per ICS feed"));
    }
    let tz = timezone(
        value(&calendar.properties, "X-WR-TIMEZONE").unwrap_or(&config.timezone),
        &config.timezone_aliases,
    )?;
    let window = options.time_window.clone().unwrap_or_else(|| {
        waymeet_core::TimeWindow::from_duration(Utc::now(), Duration::hours(48))
    });
    let mut groups: BTreeMap<String, Vec<&IcalEvent>> = BTreeMap::new();
    for event in &calendar.events {
        let uid = value(&event.properties, "UID")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("ICS event has no UID"))?;
        groups.entry(uid.to_owned()).or_default().push(event);
    }
    let mut result = Vec::new();
    for (uid, events) in groups {
        let mut master = None;
        let mut overrides = HashMap::new();
        for event in events {
            if let Some(rid) = prop(&event.properties, "RECURRENCE-ID") {
                if param(rid, "RANGE").is_some() {
                    return Err(invalid("ICS recurrence RANGE overrides are not supported"));
                }
                let original = stamp(rid, tz, config)?;
                let slot = overrides.entry(original.key()).or_insert((event, original));
                if revision(event) > revision(slot.0) {
                    *slot = (event, original);
                }
            } else if master.is_none_or(|previous| revision(event) > revision(previous)) {
                master = Some(event);
            }
        }
        if let Some(event) = master {
            if cancelled(event) {
                continue;
            }
            let begin = start(event, tz, config)?;
            let length = duration(event, begin, config)?;
            let recurring = event
                .properties
                .iter()
                .any(|p| matches!(p.name.as_str(), "RRULE" | "RDATE"));
            if recurring {
                let mut set = RRuleSet::new(begin.dt).rdate(begin.dt);
                for p in &event.properties {
                    match p.name.as_str() {
                        "RRULE" => {
                            let rule = p
                                .value
                                .as_deref()
                                .unwrap_or_default()
                                .parse::<RRule<Unvalidated>>()
                                .map_err(|_| invalid("Invalid ICS recurrence rule"))?
                                .validate(begin.dt)
                                .map_err(|_| invalid("Unsupported ICS recurrence rule"))?;
                            set = set.rrule(rule);
                        }
                        "EXDATE" | "RDATE" => {
                            for v in p.value.as_deref().unwrap_or_default().split(',') {
                                let mut p = p.clone();
                                p.value = Some(v.to_owned());
                                let date = stamp(&p, begin.dt.timezone(), config)?.dt;
                                set = if p.name == "EXDATE" {
                                    set.exdate(date)
                                } else {
                                    set.rdate(date)
                                };
                            }
                        }
                        "EXRULE" => {
                            return Err(invalid("ICS EXRULE is not supported; please use EXDATE"));
                        }
                        _ => {}
                    }
                }
                let lower = window
                    .start
                    .checked_sub_signed(length)
                    .ok_or_else(|| invalid("ICS duration exceeds query range"))?;
                let mut occurrences = set
                    .after(lower.with_timezone(&Tz::UTC))
                    .before(window.end.with_timezone(&Tz::UTC))
                    .all(10000);
                if occurrences.limited {
                    return Err(invalid("ICS recurrence expansion limit exceeded"));
                }
                occurrences.dates.sort_unstable();
                occurrences.dates.dedup();
                for date in occurrences.dates {
                    let occurrence = Stamp { dt: date, ..begin };
                    if overrides.contains_key(&occurrence.key()) {
                        continue;
                    }
                    result.push(raw_event(
                        event,
                        &uid,
                        occurrence,
                        length,
                        Some(occurrence),
                        config,
                    )?);
                }
            } else {
                result.push(raw_event(event, &uid, begin, length, None, config)?);
            }
        }
        // Overrides can move into the window from an original occurrence outside it.
        for (event, original) in overrides.values() {
            if cancelled(event) {
                continue;
            }
            let begin = start(event, tz, config)?;
            result.push(raw_event(
                event,
                &uid,
                begin,
                duration(event, begin, config)?,
                Some(*original),
                config,
            )?);
        }
    }
    fn utc(time: &RawEventTime) -> DateTime<Utc> {
        match time {
            RawEventTime::DateTime(dt) => *dt,
            RawEventTime::Date(d) => d.and_hms_opt(0, 0, 0).unwrap().and_utc(),
        }
    }
    result.retain(|e| {
        utc(&e.start) < window.end
            && (utc(&e.end) > window.start
                || (utc(&e.end) == utc(&e.start) && utc(&e.start) >= window.start))
    });
    result.sort_by(|a, b| {
        utc(&a.start)
            .cmp(&utc(&b.start))
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some(max) = options.max_results {
        result.truncate(max);
    }
    Ok(result)
}

fn revision(event: &IcalEvent) -> (u64, &str) {
    (
        value(&event.properties, "SEQUENCE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        value(&event.properties, "LAST-MODIFIED")
            .or_else(|| value(&event.properties, "DTSTAMP"))
            .unwrap_or(""),
    )
}
