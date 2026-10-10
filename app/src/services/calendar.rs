//! Calendar service — fetches and parses ICS (iCalendar) feeds.

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: String,
    pub description: String,
    pub is_all_day: bool,
    pub is_ongoing: bool,
    /// Stable id of the feed the event came from ([`source_id`]); empty in
    /// caches written before sources were tracked (fixed by the next refresh).
    #[serde(default)]
    pub source: String,
    /// The feed's display name (`X-WR-CALNAME`), e.g. "Family".
    #[serde(default)]
    pub calendar: String,
}

/// One configured calendar feed, as the page sees it (never the URL: it
/// embeds an access token).
#[derive(Debug, Clone, Serialize)]
pub struct CalendarSource {
    pub id: String,
    pub name: String,
    /// Position in the configuration: the page's colour palette index.
    pub index: usize,
    pub events: usize,
}

/// Stable, non-secret id of a feed: the first 10 hex digits of SHA-256(uri).
/// Survives reordering the feeds, unlike an index.
pub fn source_id(uri: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(uri.as_bytes()).iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// Whether `e` covers the local calendar day `date` in `tz`. DTEND is
/// exclusive: an all-day event on the 24th ends at 00:00 on the 25th and must
/// not show up on the 25th.
fn covers_day<Tz: chrono::TimeZone>(e: &CalendarEvent, date: NaiveDate, tz: &Tz) -> bool {
    let start_date = e.start.with_timezone(tz).date_naive();
    let last_instant = if e.end > e.start { e.end - chrono::Duration::seconds(1) } else { e.end };
    let end_date = last_instant.with_timezone(tz).date_naive();
    start_date == date || (start_date <= date && end_date >= date)
}

/// "Ongoing" is about now, not about when the feed was parsed.
fn with_ongoing(mut e: CalendarEvent) -> CalendarEvent {
    let now = Utc::now();
    e.is_ongoing = e.start <= now && now < e.end;
    e
}

pub struct CalendarService {
    pub events: Arc<RwLock<Vec<CalendarEvent>>>,
    ics_uris: Vec<String>,
}

impl CalendarService {
    pub fn new(ics_uris: Vec<String>) -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
            ics_uris,
        }
    }

    /// Start the background refresh loop.
    ///
    /// The very first refresh runs *inside* the spawned task rather than on the
    /// caller's task: a hostile/corrupt feed must never be able to take the
    /// process down before the HTTP listener is bound. Feed parsing is
    /// additionally panic-guarded in `refresh_impl`.
    ///
    /// The last good event set is cached on disk and loaded first: at boot the
    /// network is usually not up yet, and in the car there may be none at all,
    /// so without the cache the calendar was empty after every reboot.
    pub async fn start(&self) {
        if let Some(cached) = load_cache().await {
            info!("calendar: {} events from the offline cache", cached.len());
            *self.events.write().await = cached;
        }
        let events = self.events.clone();
        let uris = self.ics_uris.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                if refresh_impl(&events, &uris).await {
                    failures = 0;
                    save_cache(&events.read().await).await;
                } else if !uris.is_empty() {
                    failures += 1;
                }
                super::wait_for_refresh(failures, REFRESH_PERIOD).await;
            }
        });

        info!("calendar service started with {} sources", self.ics_uris.len());
    }

    /// Events overlapping `date`, where `date` is a *local* calendar day.
    ///
    /// Events are stored in UTC; comparing their UTC date against a local date
    /// mis-buckets everything within the UTC offset of midnight (in CEST an
    /// 00:30 local event is 22:30 UTC the previous day), so convert to local
    /// before taking the date.
    pub async fn get_events_for_date(&self, date: NaiveDate) -> Vec<CalendarEvent> {
        let events = self.events.read().await;
        events.iter().filter(|e| covers_day(e, date, &Local)).cloned().map(with_ongoing).collect()
    }

    /// Events per local day for `from..=to` (a multi-day event is listed on
    /// every day it covers). Days without events are absent. The span is
    /// capped at 62 days by the caller.
    pub async fn get_events_by_day(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> std::collections::BTreeMap<NaiveDate, Vec<CalendarEvent>> {
        let events = self.events.read().await;
        bucket_by_day(&events, from, to, &Local)
            .into_iter()
            .map(|(d, v)| (d, v.into_iter().cloned().map(with_ongoing).collect()))
            .collect()
    }

    /// The configured feeds with their names (from the last good refresh or
    /// the cache) and event counts, in configuration order.
    pub async fn sources(&self) -> Vec<CalendarSource> {
        let events = self.events.read().await;
        self.ics_uris
            .iter()
            .enumerate()
            .map(|(index, uri)| {
                let id = source_id(uri);
                let mine = events.iter().filter(|e| e.source == id);
                let name = mine.clone().map(|e| e.calendar.clone()).find(|n| !n.is_empty());
                CalendarSource {
                    name: name.unwrap_or_else(|| format!("Calendar {}", index + 1)),
                    events: mine.count(),
                    id,
                    index,
                }
            })
            .collect()
    }

    pub async fn get_next_event(&self) -> Option<CalendarEvent> {
        let now = Utc::now();
        let events = self.events.read().await;
        events.iter().find(|e| e.end > now).cloned()
    }
}

const REFRESH_PERIOD: std::time::Duration = std::time::Duration::from_secs(300);

fn cache_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root"))
        .join(".config/rascar/calendar-cache.json")
}

async fn load_cache() -> Option<Vec<CalendarEvent>> {
    let data = tokio::fs::read(cache_path()).await.ok()?;
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("calendar: offline cache unreadable, ignoring it: {e}");
            None
        }
    }
}

/// Private data (event titles, places): owner-only, and written to a temp
/// file then renamed so a power cut mid-write never leaves a torn cache.
async fn save_cache(events: &[CalendarEvent]) {
    let path = cache_path();
    let tmp = path.with_extension("json.tmp");
    let result = async {
        let data = serde_json::to_vec(events)?;
        tokio::fs::write(&tmp, data).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&tmp, &path).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(e) = result {
        warn!("calendar: offline cache not saved: {e:#}");
    }
}

/// Returns true when a new event set was published.
async fn refresh_impl(events: &Arc<RwLock<Vec<CalendarEvent>>>, uris: &[String]) -> bool {
    // No feeds configured is a valid state, not a refresh failure.
    if uris.is_empty() {
        return false;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // Don't follow redirects: an ICS URI must not be able to bounce us to
        // an arbitrary (possibly internal) target.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            // Don't panic the refresh task — skip this cycle and retry later.
            warn!("calendar: failed to build HTTP client: {e}");
            return false;
        }
    };

    let mut all_events = Vec::new();
    // Only publish a new event set if at least one source actually produced
    // one. Otherwise a single offline moment (tunnel, dropped hotspot) would
    // replace the last good calendar with an empty list.
    let mut any_ok = false;

    for uri in uris {
        match client.get(uri).send().await {
            Ok(resp) => match resp.text().await {
                Ok(text) => {
                    // A malformed feed must never abort the process: parsing is
                    // pure/synchronous, so contain any panic to this source.
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        parse_ics(&text, &source_id(uri))
                    })) {
                        Ok(parsed) => {
                            all_events.extend(parsed);
                            any_ok = true;
                        }
                        Err(_) => warn!("calendar: parser panicked on {}", uri_label(uri)),
                    }
                }
                Err(e) => warn!("calendar body error for {}: {e}", uri_label(uri)),
            },
            Err(e) => warn!("calendar fetch error: {e}"),
        }
    }

    if !any_ok {
        warn!("calendar: no source could be refreshed — keeping previous events");
        return false;
    }

    all_events.sort_by_key(|e| e.start);
    info!("calendar: {} events loaded", all_events.len());
    *events.write().await = all_events;
    true
}

/// First 50 *characters* of a URI, for log messages. Byte-slicing a URI can
/// land mid-UTF-8 and panic, so count chars.
fn uri_label(uri: &str) -> String {
    uri.chars().take(50).collect()
}

/// `from..=to` day buckets of the events covering each day, in `tz`.
fn bucket_by_day<'a, Tz: chrono::TimeZone>(
    events: &'a [CalendarEvent],
    from: NaiveDate,
    to: NaiveDate,
    tz: &Tz,
) -> std::collections::BTreeMap<NaiveDate, Vec<&'a CalendarEvent>> {
    let mut out = std::collections::BTreeMap::new();
    for day in from.iter_days().take_while(|d| *d <= to) {
        let list: Vec<&CalendarEvent> = events.iter().filter(|e| covers_day(e, day, tz)).collect();
        if !list.is_empty() {
            out.insert(day, list);
        }
    }
    out
}

fn parse_ics(text: &str, source: &str) -> Vec<CalendarEvent> {
    use ical::IcalParser;

    let mut events = Vec::new();
    let parser = IcalParser::new(text.as_bytes());

    for calendar in parser.flatten() {
        let calname = calendar
            .properties
            .iter()
            .find(|p| p.name == "X-WR-CALNAME")
            .and_then(|p| p.value.as_deref())
            .map(unescape_text)
            .unwrap_or_default();
        for event in calendar.events {
            let get_prop = |name: &str| -> String {
                event
                    .properties
                    .iter()
                    .find(|p| p.name == name)
                    .and_then(|p| p.value.clone())
                    .unwrap_or_default()
            };

            let summary = unescape_text(&get_prop("SUMMARY"));
            let location = unescape_text(&get_prop("LOCATION"));
            let description = unescape_text(&get_prop("DESCRIPTION"));
            let dtstart = get_prop("DTSTART");
            let dtend = get_prop("DTEND");
            let rrule = get_prop("RRULE");

            if dtstart.is_empty() {
                continue;
            }

            let is_all_day = dtstart.len() == 8; // YYYYMMDD
            let start = parse_ical_datetime(&dtstart).unwrap_or_else(Utc::now);
            let end = if dtend.is_empty() {
                start + chrono::Duration::hours(1)
            } else {
                parse_ical_datetime(&dtend).unwrap_or(start + chrono::Duration::hours(1))
            };

            let now = Utc::now();
            let base = CalendarEvent {
                title: summary,
                start,
                end,
                location,
                description,
                is_all_day,
                is_ongoing: start <= now && now <= end,
                source: source.to_string(),
                calendar: calname.clone(),
            };

            if rrule.is_empty() {
                events.push(base);
            } else {
                events.extend(expand_rrule(&rrule, base));
            }
        }
    }

    events
}

/// Pragmatic RRULE expansion for FREQ=DAILY/WEEKLY/MONTHLY/YEARLY, honoring
/// INTERVAL, COUNT and UNTIL. Occurrences are generated from the event's
/// DTSTART forward until the earliest of: COUNT reached, UNTIL passed, a
/// ~400-day look-ahead horizon, or a hard safety cap. BY* rules (BYDAY,
/// BYMONTHDAY, …) are not interpreted — the DTSTART cadence is repeated.
fn expand_rrule(rrule: &str, base: CalendarEvent) -> Vec<CalendarEvent> {
    use chrono::{Duration, Months};

    /// Upper bound on a feed-supplied INTERVAL (see the parse site below).
    const MAX_INTERVAL: i64 = 1000;

    let mut freq = String::new();
    let mut interval: i64 = 1;
    let mut count: Option<u32> = None;
    let mut until: Option<DateTime<Utc>> = None;

    for part in rrule.split(';') {
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim().to_ascii_uppercase();
        let val = kv.next().unwrap_or("").trim();
        match key.as_str() {
            "FREQ" => freq = val.to_ascii_uppercase(),
            // Clamp INTERVAL: it comes straight from an untrusted feed and
            // feeds date arithmetic that overflows (and panics) on absurd
            // values. 1000 is far beyond any legitimate recurrence.
            "INTERVAL" => interval = val.parse::<i64>().unwrap_or(1).clamp(1, MAX_INTERVAL),
            "COUNT" => count = val.parse::<u32>().ok(),
            "UNTIL" => until = parse_ical_datetime(val),
            _ => {} // WKST, BY* etc. ignored
        }
    }

    const MAX_OCCURRENCES: usize = 500;
    let horizon = Utc::now() + Duration::days(400);
    let duration = base.end - base.start;
    let now = Utc::now();

    let mut out: Vec<CalendarEvent> = Vec::new();
    let mut cur = base.start;
    let mut n: u32 = 0;

    loop {
        if let Some(c) = count {
            if n >= c {
                break;
            }
        }
        if let Some(u) = until {
            if cur > u {
                break;
            }
        }
        if cur > horizon || out.len() >= MAX_OCCURRENCES {
            break;
        }

        let end = cur + duration;
        out.push(CalendarEvent {
            start: cur,
            end,
            is_ongoing: cur <= now && now <= end,
            ..base.clone()
        });
        n += 1;

        // Every step is checked: `Duration::days/weeks` and `DateTime + Duration`
        // both panic internally on overflow, and the operands originate in an
        // untrusted feed.
        cur = match freq.as_str() {
            "DAILY" => match Duration::try_days(interval).and_then(|d| cur.checked_add_signed(d)) {
                Some(next) => next,
                None => break,
            },
            "WEEKLY" => match Duration::try_weeks(interval).and_then(|d| cur.checked_add_signed(d)) {
                Some(next) => next,
                None => break,
            },
            "MONTHLY" => match cur.checked_add_months(Months::new(interval as u32)) {
                Some(next) => next,
                None => break,
            },
            "YEARLY" => match cur.checked_add_months(Months::new((interval * 12) as u32)) {
                Some(next) => next,
                None => break,
            },
            // Unsupported/absent FREQ: emit just the first occurrence.
            _ => break,
        };
    }

    out
}

/// RFC 5545 TEXT unescaping (`\n`, `\,`, `\;`, `\\`): the ical crate
/// returns property values with their escapes intact.
fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' { out.push(c); continue; }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Parse an iCalendar date/date-time *value* into UTC.
///
/// Handles `YYYYMMDD` (DATE / all-day), `YYYYMMDDTHHmmss` (floating/local
/// time) and `YYYYMMDDTHHmmssZ` (UTC). A trailing `Z` is authoritative and
/// parsed as UTC; a value without `Z` is a *floating* local time and is
/// interpreted in the system local zone (then converted to UTC) rather than
/// being mislabeled as UTC.
///
/// LIMITATION: the `TZID=` parameter that can accompany DTSTART/DTEND is NOT
/// honored — doing so requires an IANA tz database (e.g. the `chrono-tz`
/// crate), which is not a dependency of this crate. Times carrying a TZID are
/// treated as floating/local, which is correct only when the system zone
/// matches the event's zone. Add `chrono-tz` and thread the TZID param through
/// to fully resolve this.
fn parse_ical_datetime(s: &str) -> Option<DateTime<Utc>> {
    use chrono::{Local, NaiveDateTime, TimeZone};

    let is_utc = s.ends_with('Z');
    let clean = s.replace('Z', "").replace('-', "").replace(':', "");
    if clean.len() == 8 {
        // DATE value (all-day). Anchor at local midnight so the calendar day is
        // preserved for the local viewer, then convert to UTC.
        let date = NaiveDate::parse_from_str(&clean, "%Y%m%d").ok()?;
        let naive = date.and_hms_opt(0, 0, 0)?;
        Some(
            Local
                .from_local_datetime(&naive)
                .single()
                .map(|l| l.with_timezone(&Utc))
                .unwrap_or_else(|| naive.and_utc()),
        )
    } else if clean.len() >= 15 {
        // `clean` is built from raw feed bytes: byte-slicing it can land in the
        // middle of a multi-byte character and panic. `get` returns None there.
        let dt = NaiveDateTime::parse_from_str(clean.get(..15)?, "%Y%m%dT%H%M%S").ok()?;
        if is_utc {
            Some(dt.and_utc())
        } else {
            // Floating / local time: interpret in the system local zone.
            Some(
                Local
                    .from_local_datetime(&dt)
                    .single()
                    .map(|l| l.with_timezone(&Utc))
                    .unwrap_or_else(|| dt.and_utc()),
            )
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Family\r\n\
BEGIN:VEVENT\r\nSUMMARY:Trip\r\nDTSTART;VALUE=DATE:20260928\r\nDTEND;VALUE=DATE:20260930\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:Dentist\r\nDTSTART:20260929T080000Z\r\nDTEND:20260929T090000Z\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn events_carry_their_feed_name_and_id() {
        let ev = parse_ics(FEED, "abc");
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| e.calendar == "Family" && e.source == "abc"));
    }

    #[test]
    fn source_ids_are_stable_short_and_distinct() {
        let a = source_id("https://example.com/a.ics");
        assert_eq!(a, source_id("https://example.com/a.ics"));
        assert_eq!(a.len(), 10);
        assert_ne!(a, source_id("https://example.com/b.ics"));
    }

    #[test]
    fn multi_day_events_fill_every_covered_day_but_not_the_exclusive_end() {
        // All-day dates are anchored at local midnight, so bucket in Local too.
        let ev = parse_ics(FEED, "abc");
        let d = |n| NaiveDate::from_ymd_opt(2026, 9, n).unwrap();
        let b = bucket_by_day(&ev, d(27), d(30), &Local);
        assert!(!b.contains_key(&d(27)));
        assert_eq!(b[&d(28)].len(), 1);
        assert_eq!(b[&d(29)].len(), 2); // the all-day trip + the dentist
        assert!(!b.contains_key(&d(30)), "DTEND is exclusive");
    }

    #[test]
    fn unescapes_rfc5545_text() {
        assert_eq!(unescape_text(r"Meet\nRoom 2\, floor 1\; \\ok"), "Meet\nRoom 2, floor 1; \\ok");
        assert_eq!(unescape_text("plain"), "plain");
        assert_eq!(unescape_text(r"trailing\"), r"trailing\");
    }
}
