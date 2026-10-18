use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use minijinja::{AutoEscape, Environment, UndefinedBehavior, context};
use serde_derive::Deserialize;
use sqlx::SqlitePool;
use teloxide::{
    Bot, RequestError,
    payloads::SendPhotoSetters,
    prelude::Requester,
    types::{ChatId, InputFile, MessageId, ParseMode, ThreadId},
};
use tokio_util::sync::CancellationToken;

pub const SOURCE_URL: &str =
    "https://gitea.dmz.rs/Decentrala/website/raw/branch/master/dogadjaji.csv";
pub const DEFAULT_TEMPLATE: &str = include_str!("../templates/decentrala.j2");
const LOGO: &[u8] = include_bytes!("../assets/decentrala-logo.png");

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DecentralaConfig {
    pub enabled: bool,
    pub chat_id: ChatId,
    pub thread_id: i32,
    /// Monday=1, Sunday=7.
    pub weekday: u32,
    pub time: String,
    pub timezone: String,
    /// HTTP(S) URL or local CSV path.
    pub source: String,
    /// None uses the bundled template/logo, independent of the working directory.
    pub template: Option<PathBuf>,
    pub image: Option<PathBuf>,
}

impl Default for DecentralaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chat_id: ChatId(-1002614784999),
            thread_id: 14828,
            weekday: 4,
            time: "12:00".into(),
            timezone: "Europe/Belgrade".into(),
            source: SOURCE_URL.into(),
            template: None,
            image: None,
        }
    }
}

impl DecentralaConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=7).contains(&self.weekday),
            "decentrala.weekday must be 1 (Monday) through 7 (Sunday)"
        );
        NaiveTime::parse_from_str(&self.time, "%H:%M")
            .context("invalid decentrala.time; expected HH:MM")?;
        self.timezone
            .parse::<Tz>()
            .context("invalid decentrala.timezone")?;
        ensure!(self.thread_id > 0, "decentrala.thread_id must be positive");
        ensure!(self.chat_id.0 != 0, "decentrala.chat_id must not be zero");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub date: NaiveDate,
    pub time: String,
    pub title: String,
    pub link: Option<String>,
}

pub fn events_for_date(source: &str, date: NaiveDate) -> Result<Vec<Event>> {
    ensure!(
        date.weekday() == Weekday::Mon,
        "announcement event date must be a Monday"
    );
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(source.as_bytes());
    let headers = reader
        .headers()
        .context("reading Decentrala CSV header")?
        .clone();
    let column = |name: &str| {
        headers
            .iter()
            .position(|h| h == name)
            .with_context(|| format!("Decentrala CSV missing column {name}"))
    };
    let (datum, vreme, lokacija, tema, link) = (
        column("datum")?,
        column("vreme")?,
        column("lokacija")?,
        column("tema")?,
        column("link")?,
    );
    let mut events = Vec::new();
    for (index, row) in reader.records().enumerate() {
        let row = row.with_context(|| format!("Decentrala CSV row {}", index + 2))?;
        let event_date = NaiveDate::parse_from_str(&row[datum], "%d-%m-%Y").with_context(|| {
            format!(
                "Decentrala CSV row {} invalid date {:?}",
                index + 2,
                &row[datum]
            )
        })?;
        let venue = row[lokacija]
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap_or_default();
        if event_date != date || !venue.eq_ignore_ascii_case("xecut") {
            continue;
        }
        ensure!(
            !row[tema].is_empty(),
            "Decentrala CSV row {} has no title",
            index + 2
        );
        NaiveTime::parse_from_str(&row[vreme], "%H:%M").with_context(|| {
            format!(
                "Decentrala CSV row {} invalid time {:?}",
                index + 2,
                &row[vreme]
            )
        })?;
        let event_link = if row[link].is_empty() {
            None
        } else {
            let url = reqwest::Url::parse(&row[link])
                .with_context(|| format!("Decentrala CSV row {} invalid link", index + 2))?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "Decentrala CSV row {} link must be HTTP(S)",
                index + 2
            );
            Some(row[link].to_owned())
        };
        let event = Event {
            date: event_date,
            time: row[vreme].into(),
            title: row[tema].into(),
            link: event_link,
        };
        // Repeated source records are not additional events.
        if !events
            .iter()
            .any(|e: &Event| e.time == event.time && e.title == event.title)
        {
            events.push(event);
        }
    }
    Ok(events)
}

pub fn next_monday(date: NaiveDate) -> Result<NaiveDate> {
    date.checked_add_days(Days::new(u64::from(
        7 - date.weekday().num_days_from_monday(),
    )))
    .context("next Monday is outside the supported date range")
}

/// Most recent weekly wall-clock slot. No catch-up after its target Monday starts.
pub fn due_date(config: &DecentralaConfig, now: DateTime<Utc>) -> Result<Option<NaiveDate>> {
    config.validate()?;
    let zone: Tz = config.timezone.parse()?;
    let local = now.with_timezone(&zone);
    let time = NaiveTime::parse_from_str(&config.time, "%H:%M")?;
    let days = (local.weekday().number_from_monday() + 7 - config.weekday) % 7;
    let mut slot_date = local
        .date_naive()
        .checked_sub_days(Days::new(u64::from(days)))
        .context("weekly date underflow")?;
    // At a repeated DST time choose the first occurrence; a missing time has no slot.
    let mut slot = zone
        .from_local_datetime(&slot_date.and_time(time))
        .earliest();
    if slot.is_some_and(|slot| slot > local) {
        slot_date = slot_date
            .checked_sub_days(Days::new(7))
            .context("weekly date underflow")?;
        slot = zone
            .from_local_datetime(&slot_date.and_time(time))
            .earliest();
    }
    let target = next_monday(slot_date)?;
    Ok(slot
        .filter(|slot| *slot <= local && local.date_naive() < target)
        .map(|_| target))
}

pub fn render_caption(event: &Event, template: &str) -> Result<String> {
    let mut env = Environment::new();
    // .j2 does not imply HTML escaping in MiniJinja's default callback.
    env.set_auto_escape_callback(|_| AutoEscape::Html);
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_template("decentrala.j2", template)?;
    let caption = env.get_template("decentrala.j2")?.render(context! {
        title => &event.title, date => event.date.format("%d.%m.%Y").to_string(),
        time => &event.time, link => &event.link,
    })?;
    // Conservative raw UTF-16 limit also bounds the length after HTML parsing.
    ensure!(
        caption.encode_utf16().count() <= 1024,
        "Decentrala caption exceeds Telegram's 1024-character limit; shorten the template/source title"
    );
    Ok(caption)
}

pub async fn load_source(source: &str) -> Result<String> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(client
            .get(source)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    } else {
        tokio::fs::read_to_string(source)
            .await
            .with_context(|| format!("reading CSV {source}"))
    }
}

pub async fn load_template(path: Option<&PathBuf>) -> Result<String> {
    match path {
        Some(path) => tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading template {}", path.display())),
        None => Ok(DEFAULT_TEMPLATE.into()),
    }
}

/// Reserve before any Telegram I/O. A crash leaves 'sending', never an automatic resend.
pub async fn reserve_delivery(
    pool: &SqlitePool,
    config: &DecentralaConfig,
    event: &Event,
    caption: &str,
) -> Result<bool> {
    Ok(sqlx::query("INSERT INTO decentrala_delivery (event_date,event_time,title,chat_id,thread_id,status,caption) VALUES (?,?,?,?,?,'sending',?) ON CONFLICT DO NOTHING")
        .bind(event.date.to_string()).bind(&event.time).bind(&event.title)
        .bind(config.chat_id.0).bind(config.thread_id).bind(caption)
        .execute(pool).await?.rows_affected() == 1)
}

pub async fn finish_delivery(
    pool: &SqlitePool,
    config: &DecentralaConfig,
    event: &Event,
    status: &str,
    message_id: Option<i32>,
    error: Option<&str>,
) -> Result<()> {
    let changed = sqlx::query("UPDATE decentrala_delivery SET status=?,message_id=?,error=?,updated_at=CURRENT_TIMESTAMP WHERE event_date=? AND event_time=? AND title=? AND chat_id=? AND thread_id=? AND status='sending'")
        .bind(status).bind(message_id).bind(error).bind(event.date.to_string())
        .bind(&event.time).bind(&event.title).bind(config.chat_id.0).bind(config.thread_id)
        .execute(pool).await?.rows_affected();
    ensure!(
        changed == 1,
        "Decentrala delivery reservation was unexpectedly changed"
    );
    Ok(())
}

/// Send all selected events; caller may use Bot::set_api_url for loopback testing.
pub async fn send_for_date(
    bot: &Bot,
    pool: &SqlitePool,
    config: &DecentralaConfig,
    date: NaiveDate,
    source: &str,
    template: &str,
) -> Result<()> {
    config.validate()?;
    let events = events_for_date(source, date)?;
    if events.is_empty() {
        return Ok(());
    }
    // Complete local preflight for the batch before reserving or sending any event.
    let captions = events
        .iter()
        .map(|e| render_caption(e, template))
        .collect::<Result<Vec<_>>>()?;
    let image = match &config.image {
        Some(path) => InputFile::memory(
            tokio::fs::read(path)
                .await
                .with_context(|| format!("reading logo {}", path.display()))?,
        ),
        None => InputFile::memory(LOGO),
    }
    .file_name("decentrala-logo.png");
    for (event, caption) in events.iter().zip(captions) {
        if !reserve_delivery(pool, config, event, &caption).await? {
            let status: String = sqlx::query_scalar("SELECT status FROM decentrala_delivery WHERE event_date=? AND event_time=? AND title=? AND chat_id=? AND thread_id=?")
                .bind(event.date.to_string()).bind(&event.time).bind(&event.title).bind(config.chat_id.0).bind(config.thread_id)
                .fetch_one(pool).await?;
            if status != "sent" {
                log::error!(
                    "Decentrala {} {} {:?} blocked by durable state {status}; reconcile before any resend",
                    event.date,
                    event.time,
                    event.title
                );
            }
            continue;
        }
        let result = bot
            .send_photo(config.chat_id, image.clone())
            .message_thread_id(ThreadId(MessageId(config.thread_id)))
            .caption(caption)
            .parse_mode(ParseMode::Html)
            .await;
        match result {
            Ok(message) => {
                finish_delivery(pool, config, event, "sent", Some(message.id.0), None).await?
            }
            Err(error) => {
                let status = if matches!(
                    error,
                    RequestError::Api(_)
                        | RequestError::RetryAfter(_)
                        | RequestError::MigrateToChatId(_)
                ) {
                    "failed"
                } else {
                    "uncertain"
                };
                finish_delivery(pool, config, event, status, None, Some(&error.to_string()))
                    .await?;
                log::error!(
                    "Decentrala delivery {} {} {:?}: {status}: {error}; no automatic resend",
                    event.date,
                    event.time,
                    event.title
                );
                // Other independent events still get their own delivery attempt.
            }
        }
    }
    Ok(())
}

pub async fn run(
    bot: Bot,
    pool: SqlitePool,
    config: Option<DecentralaConfig>,
    ct: CancellationToken,
) -> Result<()> {
    let Some(config) = config.filter(|config| config.enabled) else {
        return Ok(());
    };
    config.validate()?;
    let mut attempted = None;
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = ct.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        let Some(date) = due_date(&config, Utc::now())? else {
            continue;
        };
        if attempted == Some(date) {
            continue;
        }
        let attempt = async {
            let source = load_source(&config.source).await?;
            let template = load_template(config.template.as_ref()).await?;
            send_for_date(&bot, &pool, &config, date, &source, &template).await
        };
        tokio::select! {
            biased;
            _ = ct.cancelled() => return Ok(()),
            result = attempt => match result {
                Ok(()) => attempted = Some(date),
                Err(error) => log::error!("Decentrala weekly check failed: {error:#}; fetch/local errors may be retried, reserved deliveries never are"),
            }
        }
    }
}
