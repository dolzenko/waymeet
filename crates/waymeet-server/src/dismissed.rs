//! Local occurrence dismissals. The daemon serialises updates under its state lock.

use std::{fs, io, io::Write, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use waymeet_core::MeetingView;
use waymeet_protocol::MeetingsFilter;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DismissedOccurrence {
    provider_name: String,
    calendar_id: String,
    event_id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl DismissedOccurrence {
    fn matches(&self, meeting: &MeetingView) -> bool {
        self.provider_name == meeting.provider_name
            && self.calendar_id == meeting.calendar_id
            && self.event_id == meeting.id
            && self.start == meeting.start_local.with_timezone(&Utc)
    }
}

/// An in-memory store for library callers, or a durable store for the daemon.
#[derive(Debug, Default)]
pub struct DismissedEvents {
    path: Option<PathBuf>,
    entries: Vec<DismissedOccurrence>,
}

impl DismissedEvents {
    /// Loads durable state. A missing file is an empty store; invalid state is an error.
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let mut entries: Vec<DismissedOccurrence> = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        entries.retain(|entry| entry.end > Utc::now());
        Ok(Self {
            path: Some(path),
            entries,
        })
    }

    /// Whether this exact occurrence is currently dismissed.
    pub fn contains(&self, meeting: &MeetingView, now: DateTime<Utc>) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.end > now && entry.matches(meeting))
    }

    /// Saves a dismissal before changing the visible state.
    pub fn skip(&mut self, meeting: &MeetingView, now: DateTime<Utc>) -> io::Result<()> {
        let mut entries = self.active_entries(now);
        entries.push(DismissedOccurrence {
            provider_name: meeting.provider_name.clone(),
            calendar_id: meeting.calendar_id.clone(),
            event_id: meeting.id.clone(),
            start: meeting.start_local.with_timezone(&Utc),
            end: meeting.end_local.with_timezone(&Utc),
        });
        self.replace(entries)
    }

    /// Restores the most recently skipped, unexpired occurrence in these calendars.
    pub fn undo(&mut self, filter: &MeetingsFilter, now: DateTime<Utc>) -> io::Result<bool> {
        let mut entries = self.active_entries(now);
        let Some(index) = entries
            .iter()
            .rposition(|entry| filter.matches_calendar(&entry.calendar_id))
        else {
            return Ok(false);
        };
        entries.remove(index);
        self.replace(entries)?;
        Ok(true)
    }

    fn active_entries(&self, now: DateTime<Utc>) -> Vec<DismissedOccurrence> {
        self.entries
            .iter()
            .filter(|entry| entry.end > now)
            .cloned()
            .collect()
    }

    fn replace(&mut self, entries: Vec<DismissedOccurrence>) -> io::Result<()> {
        if let Some(path) = &self.path {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("dismissal file has no parent directory"))?;
            fs::create_dir_all(parent)?;
            // NamedTempFile is private (0600) and stays on the destination filesystem.
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(&mut file, &entries).map_err(io::Error::other)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(path).map_err(|error| error.error)?;
        }
        self.entries = entries;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waymeet_core::{EventTime, NormalizedEvent};

    fn meeting(now: DateTime<Utc>) -> MeetingView {
        let event = NormalizedEvent::new(
            "recurring-id",
            "Weekly demo",
            EventTime::from_utc(now),
            EventTime::from_utc(now + chrono::Duration::hours(1)),
            "work",
        );
        MeetingView::from_event_with_provider(&event, "ics:work", now)
    }

    #[test]
    fn durable_skips_are_private_expire_and_do_not_cross_occurrences() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data/dismissed-events.json");
        let now = Utc::now();
        let first = meeting(now);
        let mut store = DismissedEvents::load(path.clone()).unwrap();
        store.skip(&first, now).unwrap();
        let reloaded = DismissedEvents::load(path.clone()).unwrap();
        assert!(reloaded.contains(&first, now));
        for changed in 0..3 {
            let mut other = first.clone();
            match changed {
                0 => other.provider_name = "ics:other".into(),
                1 => other.calendar_id = "personal".into(),
                _ => other.start_local += chrono::Duration::days(7),
            }
            assert!(!reloaded.contains(&other, now));
        }
        assert!(!store.contains(&first, now + chrono::Duration::hours(1)));
        assert!(
            !store
                .undo(&MeetingsFilter::new(), now + chrono::Duration::hours(1))
                .unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn undo_is_lifo_and_calendar_scoped_even_after_reload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dismissed-events.json");
        let now = Utc::now();
        let first = meeting(now);
        let mut second = first.clone();
        second.id = "next".into();
        let mut personal = first.clone();
        personal.calendar_id = "personal".into();
        let mut store = DismissedEvents::load(path.clone()).unwrap();
        for event in [&first, &second, &personal] {
            store.skip(event, now).unwrap();
        }
        let mut store = DismissedEvents::load(path.clone()).unwrap();
        let filter = MeetingsFilter::new().include_calendars(vec!["WORK".into()]);
        assert!(store.undo(&filter, now).unwrap());
        assert!(!store.contains(&second, now));
        assert!(store.contains(&first, now));
        assert!(store.contains(&personal, now));
        assert!(store.undo(&filter, now).unwrap());
        assert!(!store.undo(&filter, now).unwrap());
        assert!(
            DismissedEvents::load(path)
                .unwrap()
                .contains(&personal, now)
        );
    }

    #[test]
    fn failed_save_or_undo_never_changes_memory_or_overwrites_bad_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dismissed-events.json");
        let now = Utc::now();
        let first = meeting(now);
        let mut next = first.clone();
        next.id = "next".into();
        let mut store = DismissedEvents::load(path.clone()).unwrap();
        store.skip(&first, now).unwrap();
        // Replacing a directory must fail even when tests run with elevated privileges.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.skip(&next, now).is_err());
        assert!(!store.contains(&next, now));
        assert!(store.undo(&MeetingsFilter::new(), now).is_err());
        assert!(store.contains(&first, now));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        fs::remove_dir(&path).unwrap();
        fs::write(&path, b"invalid state").unwrap();
        assert!(DismissedEvents::load(path.clone()).is_err());
        assert_eq!(fs::read(path).unwrap(), b"invalid state");
    }
}
