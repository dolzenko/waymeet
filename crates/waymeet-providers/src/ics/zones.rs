//! Resolve opaque TZIDs from the calendar's observances, not translated labels.
use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use ical::parser::ical::component::IcalCalendar;
use rrule::{RRule, RRuleSet, Tz, Unvalidated};

use super::{
    IcsConfig,
    parse::{invalid, param, timezone, unescape, value},
};
use crate::ProviderResult;

#[derive(Clone, Copy)]
pub(super) enum Zone<'a> {
    Named(Tz),
    Embedded(&'a Embedded),
}

impl Zone<'_> {
    // Custom zones expand recurrence in a UTC-shaped local clock; conversion
    // through observances happens for each occurrence, never by a fixed bias.
    pub fn clock(self) -> Tz {
        match self {
            Self::Named(tz) => tz,
            Self::Embedded(_) => Tz::UTC,
        }
    }
    pub fn utc(self, dt: DateTime<Tz>) -> Option<DateTime<Utc>> {
        match self {
            Self::Named(_) => Some(dt.with_timezone(&Utc)),
            Self::Embedded(zone) => zone.to_utc(dt.naive_local()),
        }
    }
    pub fn local_clock(self, utc: DateTime<Utc>) -> DateTime<Tz> {
        match self {
            Self::Named(tz) => utc.with_timezone(&tz),
            Self::Embedded(zone) => Tz::UTC.from_utc_datetime(
                &(utc.naive_utc() + Duration::seconds(i64::from(zone.offset_at(utc)))),
            ),
        }
    }
    pub fn name(self) -> String {
        match self {
            Self::Named(tz) => tz.name().to_owned(),
            Self::Embedded(_) => "VTIMEZONE".to_owned(),
        }
    }
}

struct Transition {
    at: DateTime<Utc>,
    from: i32,
    to: i32,
}

pub(super) struct Embedded {
    transitions: Vec<Transition>,
    offsets: Vec<i32>,
}

impl Embedded {
    fn offset_at(&self, utc: DateTime<Utc>) -> i32 {
        let index = self.transitions.partition_point(|t| t.at <= utc);
        if index == 0 {
            self.transitions[0].from
        } else {
            self.transitions[index - 1].to
        }
    }
    fn to_utc(&self, local: NaiveDateTime) -> Option<DateTime<Utc>> {
        // A fold uses the first occurrence, as RFC 5545 specifies. A gap has
        // no matching UTC instant and must not silently acquire a guessed bias.
        self.offsets
            .iter()
            .filter_map(|&offset| {
                let utc = local
                    .checked_sub_signed(Duration::seconds(i64::from(offset)))?
                    .and_utc();
                (self.offset_at(utc) == offset).then_some(utc)
            })
            .min()
    }
}

pub(super) struct Zones {
    embedded: BTreeMap<String, Embedded>,
}

fn key(name: &str) -> String {
    unescape(name.trim_matches('"')).to_lowercase()
}

fn offset(text: &str) -> ProviderResult<i32> {
    let bytes = text.as_bytes();
    if !matches!(bytes.len(), 5 | 7)
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(invalid("Invalid VTIMEZONE UTC offset"));
    }
    let pair = |i| i32::from(bytes[i] - b'0') * 10 + i32::from(bytes[i + 1] - b'0');
    let (h, m, s) = (pair(1), pair(3), if bytes.len() == 7 { pair(5) } else { 0 });
    if h > 23 || m > 59 || s > 59 {
        return Err(invalid("Invalid VTIMEZONE UTC offset"));
    }
    Ok((h * 3600 + m * 60 + s) * if bytes[0] == b'-' { -1 } else { 1 })
}

fn clock(text: &str) -> ProviderResult<DateTime<Tz>> {
    NaiveDateTime::parse_from_str(text, "%Y%m%dT%H%M%S")
        .map(|dt| Tz::UTC.from_utc_datetime(&dt))
        .map_err(|_| invalid("Invalid VTIMEZONE transition date"))
}

impl Zones {
    pub fn new(calendar: &IcalCalendar, window_end: DateTime<Utc>) -> ProviderResult<Self> {
        let mut year = window_end.year();
        for event in &calendar.events {
            for prop in &event.properties {
                if matches!(
                    prop.name.as_str(),
                    "DTSTART" | "DTEND" | "RECURRENCE-ID" | "RDATE" | "EXDATE"
                ) {
                    for date in prop.value.as_deref().unwrap_or_default().split(',') {
                        if let Some(n) = date.get(..4).and_then(|s| s.parse::<i32>().ok()) {
                            year = year.max(n);
                        }
                    }
                }
            }
        }
        let horizon = NaiveDate::from_ymd_opt(year + 2, 1, 1)
            .ok_or_else(|| invalid("VTIMEZONE date range is too large"))?
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut embedded = BTreeMap::new();
        for zone in &calendar.timezones {
            let id = value(&zone.properties, "TZID")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("VTIMEZONE has no TZID"))?;
            let mut transitions = Vec::new();
            let mut offsets = Vec::new();
            for part in &zone.transitions {
                let required = |name| {
                    value(&part.properties, name)
                        .ok_or_else(|| invalid("Incomplete VTIMEZONE observance"))
                };
                let start = clock(required("DTSTART")?)?;
                let from = offset(required("TZOFFSETFROM")?)?;
                let to = offset(required("TZOFFSETTO")?)?;
                offsets.extend([from, to]);
                let mut dates = vec![start];
                for prop in &part.properties {
                    match prop.name.as_str() {
                        "RRULE" => {
                            let mut rule = prop
                                .value
                                .as_deref()
                                .unwrap_or_default()
                                .parse::<RRule<Unvalidated>>()
                                .map_err(|_| invalid("Invalid VTIMEZONE recurrence rule"))?;
                            if let Some(until) = rule.get_until() {
                                // RFC UNTIL is UTC; onset recurrence uses local time
                                // before the transition (TZOFFSETFROM).
                                rule = rule
                                    .clone()
                                    .until(*until + Duration::seconds(i64::from(from)));
                            }
                            let rule = rule
                                .validate(start)
                                .map_err(|_| invalid("Unsupported VTIMEZONE recurrence rule"))?;
                            let expanded = RRuleSet::new(start)
                                .rrule(rule)
                                .before(Tz::UTC.from_utc_datetime(&horizon))
                                .all(10000);
                            if expanded.limited {
                                return Err(invalid("VTIMEZONE expansion limit exceeded"));
                            }
                            dates.extend(expanded.dates);
                        }
                        "RDATE" => {
                            if param(prop, "VALUE").is_some_and(|v| v != "DATE-TIME") {
                                return Err(invalid("Unsupported VTIMEZONE RDATE value"));
                            }
                            for date in prop.value.as_deref().unwrap_or_default().split(',') {
                                dates.push(clock(date)?);
                            }
                        }
                        "EXRULE" | "EXDATE" => {
                            return Err(invalid("Unsupported VTIMEZONE exclusion rule"));
                        }
                        _ => {}
                    }
                }
                for date in dates {
                    let at = date.with_timezone(&Utc) - Duration::seconds(i64::from(from));
                    transitions.push(Transition { at, from, to });
                }
                if transitions.len() > 10000 {
                    return Err(invalid("VTIMEZONE expansion limit exceeded"));
                }
            }
            if transitions.is_empty() {
                return Err(invalid("VTIMEZONE has no observances"));
            }
            transitions.sort_by_key(|t| t.at);
            if transitions
                .windows(2)
                .any(|t| t[0].at == t[1].at && (t[0].from != t[1].from || t[0].to != t[1].to))
            {
                return Err(invalid("Conflicting VTIMEZONE transitions"));
            }
            transitions.dedup_by_key(|t| t.at);
            offsets.sort_unstable();
            offsets.dedup();
            if embedded
                .insert(
                    key(id),
                    Embedded {
                        transitions,
                        offsets,
                    },
                )
                .is_some()
            {
                return Err(invalid("Duplicate VTIMEZONE identifier"));
            }
        }
        Ok(Self { embedded })
    }

    pub fn resolve<'a>(&'a self, name: &str, config: &IcsConfig) -> ProviderResult<Zone<'a>> {
        let canonical = key(name);
        // Explicit user overrides win; otherwise definitions in this calendar
        // are authoritative, including for an IANA or Windows-looking TZID.
        if let Some((_, target)) = config
            .timezone_aliases
            .iter()
            .find(|(id, _)| key(id) == canonical)
        {
            return timezone(target, &BTreeMap::new()).map(Zone::Named);
        }
        if let Some(zone) = self.embedded.get(&canonical) {
            return Ok(Zone::Embedded(zone));
        }
        timezone(&unescape(name), &BTreeMap::new()).map(Zone::Named)
    }
}
