use parking_lot::Mutex;
use std::sync::Arc;

use axum::{Router, body::Bytes, extract::State, http::header, routing::post};
use chrono::{NaiveDate, TimeZone, Utc};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use teloxide::Bot;
use tokio_util::sync::CancellationToken;
use xecut_bot::decentrala::{
    DEFAULT_TEMPLATE, DecentralaConfig, due_date, events_for_date, finish_delivery, next_monday,
    render_caption, reserve_delivery, send_for_date,
};

const FIXTURE: &str = include_str!("fixtures/decentrala.csv");
fn monday() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
}

#[test]
fn multiple_events_and_venue_boundaries() {
    let events = events_for_date(FIXTURE, monday()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].title, "Zaobilazenje, cenzure");
    assert_eq!(events[1].time, "20:30");
    let rows = "datum,vreme,lokacija,tema,link\n19-10-2026,19:00,Xecutopia,Wrong,\n19-10-2026,19:00,Other https://xecut.me,Xecut,\n19-10-2026,19:00,Xecut,Right,\n19-10-2026,19:00,Xecut,Right,\n";
    assert_eq!(events_for_date(rows, monday()).unwrap().len(), 1);
}

#[test]
fn empty_and_corrupt_sources_do_not_create_events() {
    assert!(
        events_for_date("datum,vreme,lokacija,tema,link\n", monday())
            .unwrap()
            .is_empty()
    );
    assert!(events_for_date("", monday()).is_err());
    let bad = "datum,vreme,lokacija,tema,link\n31-02-2026,19:00,Other,Bad,\n";
    let error = format!("{:#}", events_for_date(bad, monday()).unwrap_err());
    assert!(error.contains("row 2"));
    assert!(error.contains("31-02-2026"));
    assert!(events_for_date(FIXTURE, monday().succ_opt().unwrap()).is_err());
}

#[test]
fn source_values_and_links_are_html_escaped() {
    let mut event = events_for_date(FIXTURE, monday()).unwrap().remove(0);
    event.title = "<script> & \"source\"".into();
    let caption = render_caption(&event, DEFAULT_TEMPLATE).unwrap();
    assert!(!caption.contains("<script>"));
    assert!(caption.contains("&lt;script&gt;"));
    assert!(caption.contains("a=1&amp;b=2"));
    assert!(caption.contains("19.10.2026"));
    assert!(caption.contains("19:00"));
    assert!(caption.contains("https://xecut.me/ru/location/"));
    event.link = None;
    assert!(
        !render_caption(&event, DEFAULT_TEMPLATE)
            .unwrap()
            .contains("forum.dmz.rs")
    );
    assert!(render_caption(&event, "{{ nonexistent }}").is_err());
    assert!(render_caption(&event, &"x".repeat(1025)).is_err());
    let unsafe_link =
        "datum,vreme,lokacija,tema,link\n19-10-2026,19:00,Xecut,Title,javascript:alert(1)\n";
    assert!(events_for_date(unsafe_link, monday()).is_err());
}

#[test]
fn next_monday_crosses_month_and_year_and_is_strict() {
    for (input, expected) in [
        ("2026-12-31", "2027-01-04"),
        ("2026-10-29", "2026-11-02"),
        ("2026-10-19", "2026-10-26"),
    ] {
        let date = NaiveDate::parse_from_str(input, "%Y-%m-%d").unwrap();
        assert_eq!(next_monday(date).unwrap().to_string(), expected);
    }
}

#[test]
fn thursday_schedule_tracks_belgrade_dst_and_no_late_monday_catchup() {
    let config = DecentralaConfig::default();
    for (month, day, utc_hour, target) in [
        (3, 26, 11, "2026-03-30"),
        (4, 2, 10, "2026-04-06"),
        (10, 22, 10, "2026-10-26"),
        (10, 29, 11, "2026-11-02"),
    ] {
        let before = Utc
            .with_ymd_and_hms(2026, month, day, utc_hour - 1, 59, 59)
            .unwrap();
        let due = Utc
            .with_ymd_and_hms(2026, month, day, utc_hour, 0, 0)
            .unwrap();
        assert_eq!(due_date(&config, before).unwrap(), None);
        assert_eq!(due_date(&config, due).unwrap().unwrap().to_string(), target);
    }
    assert_eq!(
        due_date(
            &config,
            Utc.with_ymd_and_hms(2026, 10, 19, 10, 0, 0).unwrap()
        )
        .unwrap(),
        None
    );
    let mut sunday = config;
    sunday.weekday = 7;
    sunday.time = "02:30".into();
    // Spring's missing wall time is skipped, not shifted to an invented time.
    assert_eq!(
        due_date(&sunday, Utc.with_ymd_and_hms(2026, 3, 29, 2, 0, 0).unwrap()).unwrap(),
        None
    );
    // Autumn's repeated wall time is the same event date on both occurrences.
    let first = due_date(
        &sunday,
        Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap(),
    )
    .unwrap();
    let second = due_date(
        &sunday,
        Utc.with_ymd_and_hms(2026, 10, 25, 1, 30, 0).unwrap(),
    )
    .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.unwrap().to_string(), "2026-10-26");
}

async fn database() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn reservations_are_atomic_and_failures_never_auto_retry() {
    let pool = database().await;
    let config = DecentralaConfig::default();
    let event = events_for_date(FIXTURE, monday()).unwrap().remove(0);
    let (a, b) = tokio::join!(
        reserve_delivery(&pool, &config, &event, "caption"),
        reserve_delivery(&pool, &config, &event, "caption")
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert!(
        !reserve_delivery(&pool, &config, &event, "changed caption")
            .await
            .unwrap()
    );
    finish_delivery(&pool, &config, &event, "uncertain", None, Some("timeout"))
        .await
        .unwrap();
    assert!(
        !reserve_delivery(&pool, &config, &event, "caption")
            .await
            .unwrap()
    );
    assert!(
        finish_delivery(&pool, &config, &event, "sent", Some(1), None)
            .await
            .is_err()
    );
}

async fn fake_telegram(
    response: &'static str,
) -> (Bot, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let app = Router::new()
        .route(
            "/{*path}",
            post(
                move |State(requests): State<Arc<Mutex<Vec<String>>>>, body: Bytes| async move {
                    requests
                        .lock()
                        .push(String::from_utf8_lossy(&body).into_owned());
                    ([(header::CONTENT_TYPE, "application/json")], response)
                },
            ),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (Bot::new("123:test").set_api_url(url), requests, task)
}

#[tokio::test]
async fn real_photo_dispatch_is_durable_across_database_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("state.sqlite").display()
    );
    let pool = SqlitePool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let (bot, requests, task) = fake_telegram(r#"{"ok":true,"result":{"message_id":91,"date":1792072800,"chat":{"id":-100777888999,"type":"supergroup","title":"Residents"},"text":"delivered"}}"#).await;
    let config = DecentralaConfig {
        chat_id: teloxide::types::ChatId(-100777888999),
        thread_id: 24680,
        image: Some(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/decentrala-logo.png"),
        ),
        ..Default::default()
    };
    send_for_date(&bot, &pool, &config, monday(), FIXTURE, DEFAULT_TEMPLATE)
        .await
        .unwrap();
    assert_eq!(requests.lock().len(), 2);
    let body = requests.lock()[0].clone();
    for expected in [
        "-100777888999",
        "24680",
        "decentrala-logo.png",
        "Zaobilazenje, cenzure",
        "19.10.2026",
        "HTML",
    ] {
        assert!(body.contains(expected), "missing {expected}");
    }
    pool.close().await;
    let reopened = SqlitePool::connect(&url).await.unwrap();
    send_for_date(
        &bot,
        &reopened,
        &config,
        monday(),
        FIXTURE,
        DEFAULT_TEMPLATE,
    )
    .await
    .unwrap();
    assert_eq!(requests.lock().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM decentrala_delivery WHERE status='sent' AND message_id=91"
        )
        .fetch_one(&reopened)
        .await
        .unwrap(),
        2
    );
    task.abort();
}

#[tokio::test]
async fn accepted_but_unreadable_response_is_uncertain_and_not_resent() {
    let pool = database().await;
    let (bot, requests, task) = fake_telegram("response lost after acceptance").await;
    let config = DecentralaConfig::default();
    for _ in 0..2 {
        send_for_date(&bot, &pool, &config, monday(), FIXTURE, DEFAULT_TEMPLATE)
            .await
            .unwrap();
    }
    assert_eq!(requests.lock().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM decentrala_delivery WHERE status='uncertain'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    task.abort();
}

#[tokio::test]
async fn explicit_rejection_is_failed_and_no_events_send_nothing() {
    let pool = database().await;
    let (bot, requests, task) =
        fake_telegram(r#"{"ok":false,"error_code":400,"description":"Bad Request: topic closed"}"#)
            .await;
    let config = DecentralaConfig::default();
    send_for_date(
        &bot,
        &pool,
        &config,
        monday(),
        "datum,vreme,lokacija,tema,link\n",
        DEFAULT_TEMPLATE,
    )
    .await
    .unwrap();
    assert!(requests.lock().is_empty());
    for _ in 0..2 {
        send_for_date(&bot, &pool, &config, monday(), FIXTURE, DEFAULT_TEMPLATE)
            .await
            .unwrap();
    }
    assert_eq!(requests.lock().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM decentrala_delivery WHERE status='failed'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    task.abort();
}

#[tokio::test]
async fn disabled_and_cancelled_tasks_do_not_fetch() {
    let pool = database().await;
    let config = DecentralaConfig {
        source: "/nonexistent/source.csv".into(),
        ..Default::default()
    };
    xecut_bot::decentrala::run(
        Bot::new("123:test"),
        pool.clone(),
        Some(config.clone()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let config = DecentralaConfig {
        enabled: true,
        ..config
    };
    xecut_bot::decentrala::run(Bot::new("123:test"), pool, Some(config), token)
        .await
        .unwrap();
}

#[test]
fn legacy_config_remains_valid_without_announcements() {
    let config = xecut_bot::Config::new(
        "DECENTRALA_CONFIG_TEST",
        vec!["xecut_bot_example.yaml".into()],
    )
    .unwrap();
    assert!(!config.telegram_bot.decentrala.unwrap().enabled);
    let legacy = config::Config::builder().add_source(config::File::from_str("telegram_bot:\n  bot_token: ''\n  public_chat_id: 0\n  private_chat_id: 0\n  public_channel_id: 0\n  alert_chat_id: 0\ndb:\n  sqlite_path: ':memory:'", config::FileFormat::Yaml)).build().unwrap().try_deserialize::<xecut_bot::Config>().unwrap();
    assert!(legacy.telegram_bot.decentrala.is_none());
}

#[test]
fn offline_preview_executes_without_configuration_or_credentials() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_xecut_bot"))
        .args([
            "--config",
            "/nonexistent/bot.yaml",
            "decentrala-preview",
            "--date",
            "2026-10-15",
            "--source",
        ])
        .arg(root.join("tests/fixtures/decentrala.csv"))
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captions = String::from_utf8(output.stdout).unwrap();
    assert!(captions.contains("Zaobilazenje, cenzure"));
    assert!(captions.contains("Distribuirane mreze"));
    assert!(!captions.contains("Xecut elsewhere"));
    assert!(!captions.contains("Next week"));
}

#[tokio::test]
async fn batch_render_failure_happens_before_any_send_or_reservation() {
    let pool = database().await;
    let (bot, requests, task) = fake_telegram("should never be requested").await;
    let config = DecentralaConfig::default();
    let source = format!(
        "datum,vreme,lokacija,tema,link\n19-10-2026,19:00,Xecut,Normal,\n19-10-2026,20:00,Xecut,{},\n",
        "x".repeat(1025)
    );
    assert!(
        send_for_date(&bot, &pool, &config, monday(), &source, DEFAULT_TEMPLATE)
            .await
            .is_err()
    );
    assert!(requests.lock().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM decentrala_delivery")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    task.abort();
}
