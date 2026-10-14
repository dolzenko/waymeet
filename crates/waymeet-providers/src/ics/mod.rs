//! Read-only HTTP iCalendar subscriptions. Feed URLs are treated as secrets.
mod parse;
mod zones;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use tokio::sync::Mutex;
use url::Url;

use crate::{
    BoxFuture, CalendarInfo, CalendarProvider, FetchOptions, FetchResult, ProviderError,
    ProviderResult, ProviderStatus,
};

/// Subscription configuration. The URL is deliberately omitted from Debug output.
#[derive(Clone)]
pub struct IcsConfig {
    pub name: String,
    url: Url,
    pub timezone: String,
    pub timezone_aliases: BTreeMap<String, String>,
    pub user_agent: String,
}

impl IcsConfig {
    pub fn new(name: &str, url: &str) -> ProviderResult<Self> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            return Err(ProviderError::configuration(
                "ICS name must contain only letters, digits, - or _",
            ));
        }
        let url = url
            .strip_prefix("webcal://")
            .map(|rest| format!("https://{rest}"))
            .unwrap_or_else(|| url.to_owned());
        let url = Url::parse(&url).map_err(|_| ProviderError::configuration("Invalid ICS URL"))?;
        if !matches!(url.scheme(), "https" | "http")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProviderError::configuration(
                "ICS URL must use HTTP(S) without embedded credentials",
            ));
        }
        Ok(Self {
            name: name.into(),
            url,
            timezone: "UTC".into(),
            timezone_aliases: BTreeMap::new(),
            user_agent: "WayMeet/0.1".into(),
        })
    }

    pub fn validate(&self) -> ProviderResult<()> {
        parse::timezone(&self.timezone, &self.timezone_aliases)?;
        for target in self.timezone_aliases.values() {
            parse::timezone(target, &BTreeMap::new())?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Cache {
    content: Option<String>,
    etag: Option<reqwest::header::HeaderValue>,
    modified: Option<reqwest::header::HeaderValue>,
    last_sync: Option<chrono::DateTime<Utc>>,
}

/// A single independently cached subscription, identified by name rather than URL.
pub struct IcsProvider {
    config: IcsConfig,
    name: String,
    client: reqwest::Client,
    cache: Mutex<Cache>,
    authenticated: AtomicBool,
}

impl IcsProvider {
    pub fn new(config: IcsConfig) -> ProviderResult<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(&config.user_agent)
            .build()
            .map_err(|_| ProviderError::configuration("Could not initialise ICS HTTP client"))?;
        Ok(Self {
            name: format!("ics:{}", config.name),
            config,
            client,
            cache: Mutex::new(Cache::default()),
            authenticated: AtomicBool::new(false),
        })
    }

    async fn fetch(&self, options: FetchOptions) -> ProviderResult<FetchResult> {
        if options
            .calendar_ids
            .as_ref()
            .is_some_and(|ids| !ids.contains(&self.config.name))
        {
            return Ok(FetchResult::with_events(Vec::new()));
        }
        let mut cache = self.cache.lock().await;
        let mut request = self
            .client
            .get(self.config.url.clone())
            .header("Accept", "text/calendar,text/plain,*/*")
            .header("Accept-Language", "en-US,en;q=0.5");
        if let Some(etag) = &cache.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(modified) = &cache.modified {
            request = request.header(IF_MODIFIED_SINCE, modified);
        }
        // reqwest errors include the URL, which may itself grant calendar access.
        let mut response = request
            .send()
            .await
            .map_err(|_| ProviderError::network("ICS download failed"))?;
        let unchanged = response.status() == reqwest::StatusCode::NOT_MODIFIED;
        let etag = response.headers().get(ETAG).cloned();
        let modified = response.headers().get(LAST_MODIFIED).cloned();
        let content = if unchanged {
            cache.content.clone().ok_or_else(|| {
                ProviderError::invalid_response("ICS returned 304 without a cached calendar")
            })?
        } else {
            let status = response.status();
            match status.as_u16() {
                200 => {}
                401 => {
                    return Err(ProviderError::authentication(
                        "ICS feed requires authentication",
                    ));
                }
                403 => return Err(ProviderError::authorization("ICS feed access denied")),
                429 => return Err(ProviderError::rate_limited("ICS feed rate limit reached")),
                _ => {
                    return Err(ProviderError::network(format!(
                        "ICS download returned HTTP {status}"
                    )));
                }
            }
            const MAX_BYTES: usize = 8 * 1024 * 1024;
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ProviderError::network("Could not read ICS response"))?
            {
                if bytes.len() + chunk.len() > MAX_BYTES {
                    return Err(ProviderError::invalid_response("ICS feed exceeds 8 MiB"));
                }
                bytes.extend_from_slice(&chunk);
            }
            String::from_utf8(bytes)
                .map_err(|_| ProviderError::invalid_response("ICS feed is not UTF-8"))?
        };
        // Re-expand even on 304: the query window moves whilst the feed stays unchanged.
        // Commit cache validators only after the entire calendar has been accepted.
        let events = parse::events(&content, &self.config, &options)?;
        if !unchanged {
            cache.content = Some(content);
            cache.etag = etag;
            cache.modified = modified;
        }
        cache.last_sync = Some(Utc::now());
        self.authenticated.store(true, Ordering::Relaxed);
        Ok(FetchResult::with_events(events))
    }
}

impl CalendarProvider for IcsProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch_events(&self, options: FetchOptions) -> BoxFuture<'_, ProviderResult<FetchResult>> {
        Box::pin(self.fetch(options))
    }
    fn list_calendars(&self) -> BoxFuture<'_, ProviderResult<Vec<CalendarInfo>>> {
        Box::pin(async {
            Ok(vec![
                CalendarInfo::new(&self.config.name, &self.config.name)
                    .with_timezone(&self.config.timezone),
            ])
        })
    }
    fn status(&self) -> BoxFuture<'_, ProviderStatus> {
        Box::pin(async {
            let mut status = ProviderStatus::new("ics");
            status.is_authenticated = self.is_authenticated();
            status.last_sync = self.cache.lock().await.last_sync;
            status.calendar_count = 1;
            status
        })
    }
    fn refresh_auth(&self) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
