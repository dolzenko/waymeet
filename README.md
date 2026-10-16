[![codecov](https://codecov.io/gh/chmouel/waymeet/graph/badge.svg?token=P3JYJCCSNF)](https://codecov.io/gh/chmouel/waymeet)

# WayMeet

WayMeet is a Linux calendar tool with a CLI/daemon architecture. It reads
Google Calendar and CalDAV, shows upcoming meetings in the terminal or Waybar,
and supports quick meeting actions.

## Features

- Google Calendar, CalDAV, and read-only ICS subscription support.
- Terminal, JSON, and Waybar outputs.
- Google event actions: edit in calendar, decline, and delete.
- Detects meeting links for Zoom, Meet, Teams, Webex, Jitsi, and more.
- Schedules desktop notifications with snooze support.
- Provides action commands for joining meetings, copying meeting details,
  refreshing providers, and creating meeting links.

## Installation

### From source

No releases and packages binaries are published yet. Install from source.

Install the CLI:

```sh
cargo install --path crates/waymeet-client
```

### Arch Linux package (this fork)

From `packaging/arch`, run `makepkg -si` to build and install the current checkout
as `waymeet-ics`. This installs `/usr/bin/waymeet` and a user systemd unit. After
configuring a provider, set `[server] auto_start = false` and run
`systemctl --user enable --now waymeet.service` for supervised background sync.

## Quick Start

To show each calendar separately whilst sharing one daemon and configuration:

```sh
waymeet --calendar work --waybar
waymeet --calendar google --waybar
waymeet --calendar work --open-meet-url
```

`--calendar ID` selects calendars whose IDs contain `ID`, ignoring case,
using the same matching rule as `filters.include_calendars`. Repeat the flag
to include several calendars. It overrides the configured inclusion list;
other filters still apply. The selection covers the chip, daily tooltip and
meeting actions, whilst the daemon continues synchronising every source.

### Google Calendar

Run the setup wizard:

```sh
waymeet auth google --guide
```

Manual setup:

1. Create a Google OAuth client of type `Desktop app` (a `Web
   application` client will fail with `redirect_uri_mismatch`).
2. Download the credentials JSON file.
3. Run:

```sh
waymeet auth google --account work \
  --credentials-file /path/to/client_secret_<id>.json
```

By default, waymeet requests `calendar.events` (read/write events) and
`calendar.readonly` (list calendars). This enables decline and delete actions.
Use `--read-only` (or `read_only = true` in account config) for read-only
access.

Authentication commands:

```sh
waymeet auth                          # starts Google guide when no providers are configured
waymeet auth status                   # inspect all accounts and tokens
waymeet auth logout --account work    # clear stored tokens
waymeet auth logout --revoke          # also revoke them with Google
waymeet auth google --no-browser      # headless/SSH flow, paste redirect URL in terminal
```

If you re-authenticate whilst the daemon is running, the CLI asks the daemon to
refresh with the new tokens.

WayMeet stores tokens in `~/.local/share/waymeet/` with `0600` permissions by
default. To use the desktop keyring instead (freedesktop Secret Service, via
`secret-tool` from `libsecret`), set
`token_storage = "keyring"` on the account.

### CalDAV

Add a CalDAV section to `~/.config/waymeet/config.toml`:

```toml
[caldav]
url = "https://caldav.example.com/calendars/chmouel/"
username = "env::WAYMEET_CALDAV_USER"
password = "env::WAYMEET_CALDAV_PASSWORD"
calendar_hint = "work"
```

### ICS subscriptions (including Outlook/OWA)

Subscribe directly to an HTTP(S) or `webcal://` iCalendar feed; no CalDAV server
is required. Each subscription has a unique name and is read-only:

```toml
[[ics]]
name = "work"
url_file = "/home/you/.config/waymeet/work-ics-url"
timezone = "Europe/London"
# Alternatively: url = "env::WAYMEET_ICS_URL" (also accepts pass:: references)
# Some published OWA feeds require a browser-style User-Agent:
# user_agent = "Mozilla/5.0 (X11; Linux x86_64; rv:143.0) Gecko/20100101 Firefox/143.0"

# Map localised or custom TZIDs explicitly when necessary:
# [ics.timezone_aliases]
# "Custom Office Time" = "Europe/London"
```

Store only the feed URL in `url_file` and restrict its permissions to `0600`.
Published calendar URLs may grant access without a password. WayMeet never uses
these URLs as calendar IDs or includes them in provider errors. `webcal://`
becomes HTTPS. HTTP feeds use TLS verification, a 30-second timeout, an 8 MiB
response limit, and conditional requests using ETag/Last-Modified.

ICS subscriptions use the daemon's existing sync interval. Recurring events
are expanded locally for the requested time window, including `RRULE`, `RDATE`,
`EXDATE`, and individual `RECURRENCE-ID` edits/cancellations. Occurrence IDs remain
stable when a meeting is moved. All-day recurrences accept date-only `UNTIL`
limits, including the last date as exported by Google Calendar. Windows timezone
names are mapped through Unicode CLDR; IANA timezone rules preserve local meeting times across DST changes.
Floating times use `X-WR-TIMEZONE`, or the configured `timezone` (UTC by default).
Embedded `VTIMEZONE` definitions resolve arbitrary Microsoft/OWA identifiers,
including translated display labels and escaped commas. STANDARD/DAYLIGHT
observances, RRULE/RDATE transitions, and fractional offsets are evaluated;
recurring meetings retain their local time across daylight-saving changes.
Explicit `timezone_aliases` override embedded definitions; otherwise embedded
rules take precedence over IANA/Windows mappings. No offset is guessed from a
label. Unknown identifiers without usable rules still report an error. Start-relative DISPLAY/AUDIO alarms
supply reminder offsets; otherwise the ordinary notification fallback applies.

Unsupported recurrence features (`RANGE=THISANDFUTURE`, `RDATE;VALUE=PERIOD`,
`EXRULE`), unknown timezones, malformed calendars, and expansion-limit failures
report a provider error and preserve the last good events. A valid empty feed
clears them. Cached feeds are re-expanded even after HTTP 304, so meetings enter
and leave the moving query window correctly. Feed changes cannot be written back;
Google-only decline/delete actions are unavailable for ICS subscriptions.

### Run it

Show the next meeting:

```sh
waymeet
```

Use Waybar output:

```sh
waymeet --waybar
```

The CLI starts the daemon automatically when needed.

## Common Commands

- `waymeet --open-meet-url`
- `waymeet --copy-meeting-url`
- `waymeet --copy-meeting-id`
- `waymeet --copy-meeting-passcode`
- `waymeet --open-calendar-day`
- `waymeet --open-link-from-clipboard`
- `waymeet --create meet|zoom|teams|gcal`
- `waymeet --json status` — machine-readable daemon and provider health
- `waymeet --refresh`
- `waymeet --snooze N`

## Configuration

Default config path:

`~/.config/waymeet/config.toml`

Use `config.example.toml` as a template. Validate the resulting file with:

```sh
waymeet config validate
```

For a countdown followed by the meeting title, use `format = "{when} · {title}"`
in `[display]`. The time placeholder is `{when}`; `{start_time}` and `{end_time}`
provide clock times. The same placeholders work in `tooltip_format`.

To keep finished events from today in the Waybar tooltip, set
`tooltip_include_past_today = true` in `[display]`. The chip and meeting actions
still select ongoing/upcoming events. Upcoming events from the next 24 hours
remain in the tooltip. Omit `tooltip_limit` to show the full list; ordinary
calendar/title filters still apply. Restart the daemon after upgrading: it now
fetches from local midnight through the next 24 hours to retain today's agenda.

Supply Google credentials in either of these forms:

- `credentials_file = "/path/to/client_secret_<id>.json"` for the downloaded
  Google OAuth Desktop App JSON
- `client_id` and `client_secret` values, including `env::` and `pass::`
  references

CalDAV credentials also support `env::` and `pass::` references.

Tune the daemon cadence in the `[server]` section:

- `sync_interval_secs` — interval between calendar syncs (default 300, minimum 30)
- `refresh_cooldown_secs` — cooldown after a manual refresh (default 30)
- `notify_tick_secs` — interval between notification checks (default 30, minimum 5)

Notification checks run on their own ticker, independent of the sync interval,
so reminders can fire on time even when calendar syncs run less often.

### Notification timing

For Google Calendar events, WayMeet reads the event's own `reminders` setting
(popup reminders configured in Google Calendar, on by default) and uses those
minutes-before-start values before the global `notifications.minutes_before`
configuration. Explicit reminder overrides take precedence. If the event relies
on the calendar's default reminders, WayMeet resolves those defaults. WayMeet
falls back to `notifications.minutes_before` only when an event has disabled
reminders altogether, or when no provider reminder can be resolved (for example,
with non-Google providers).

## Environment Variables

- `WAYMEET_CONFIG`
- `WAYMEET_SOCKET`
- `GOOGLE_CLIENT_ID`
- `GOOGLE_CLIENT_SECRET`
- `GOOGLE_CREDENTIALS_FILE`
- `RUST_LOG`

## Architecture

The CLI communicates with a background daemon over a Unix socket. The daemon
handles provider polling, caching, and notifications.

For implementation details, see `DESIGN.md`.

## Licence

Apache-2.0

### Skipping an occurrence locally

Use `waymeet --calendar ID --skip` to dismiss the occurrence currently selected
for the display. The next eligible meeting is shown immediately; joining actions
and reminders also exclude the skipped occurrence. Your calendar invitation and
attendance response remain unchanged.

Use `waymeet --calendar ID --undo-skip` to restore the latest unexpired skip in
that calendar. Repeated undo restores earlier skips in reverse order. Omit
`--calendar` to use the configured calendar selection. Other display filters do
not prevent undoing a skip.

Skips are stored privately in `$XDG_DATA_HOME/waymeet/dismissed-events.json`
(default `~/.local/share/waymeet/dismissed-events.json`). They survive daemon
restarts and calendar refreshes, and expire when the occurrence ends. A changed
start time is a new occurrence. Recurring meetings and identical IDs in other
providers or calendars remain independent. A failed save leaves the previous
state intact; invalid saved state is reported during daemon startup.
