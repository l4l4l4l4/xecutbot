use chrono::NaiveDate;
use xecut_bot::{
    config::MoneyConfig,
    money::{Money, Month, Status, parse_euros, reaction_approved},
};

fn config() -> MoneyConfig {
    MoneyConfig {
        enabled: true,
        minimum_monthly_euros: Some("10".into()),
        financier_user_id: Some(42),
        start_month: Some("2026-01".into()),
        minimum_effective_month: None,
        initial_member_ids: vec![7, 42],
        initial_roster_complete: true,
        chart_months: 12,
        contact: "@rvbar_el3".into(),
    }
}
async fn ledger() -> Money {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let m = Money::new(pool, config()).unwrap();
    m.initialize().await.unwrap();
    m
}
#[tokio::test]
async fn claims_are_pending_until_numeric_financier_confirms_once() {
    let m = ledger().await;
    let months = vec![
        Month::parse("2026-02").unwrap(),
        Month::parse("2026-03").unwrap(),
    ];
    let c = m.claim(-1, 1, 7, 2001, &months).await.unwrap();
    assert_eq!(m.balance().await.unwrap(), 0);
    assert_eq!(m.status(7, months[0]).await.unwrap(), Status::Pending);
    assert_eq!(m.claim(-1, 1, 7, 1, &months).await.unwrap().id, c.id);
    m.attach_card(c.id, 99).await.unwrap();
    assert!(!m.confirm(-1, 99, 8).await.unwrap());
    assert!(m.confirm(-1, 99, 42).await.unwrap());
    assert!(!m.confirm(-1, 99, 42).await.unwrap());
    assert_eq!(m.balance().await.unwrap(), 2001);
    assert_eq!(m.paid_cents(7, months[0]).await.unwrap(), 1001);
    assert_eq!(m.paid_cents(7, months[1]).await.unwrap(), 1000);
    assert_eq!(m.receipts().await.unwrap().len(), 1);
    let restored = Money::new(m.pool.clone(), config()).unwrap();
    restored.initialize().await.unwrap();
    assert_eq!(restored.receipts().await.unwrap().len(), 1);
    assert_eq!(restored.balance().await.unwrap(), 2001);
    m.acknowledged(c.id).await.unwrap();
    assert!(m.receipts().await.unwrap().is_empty());
    let id = m.history(20).await.unwrap()[0].id;
    m.reverse(-1, 2, 42, id, "mistake").await.unwrap();
    assert_eq!(m.balance().await.unwrap(), 0);
    assert_eq!(m.status(7, months[0]).await.unwrap(), Status::Due);
    assert!(m.reverse(-1, 3, 42, id, "again").await.is_err());
}
#[tokio::test]
async fn membership_intervals_keep_history_without_backdating_new_members() {
    let m = ledger().await;
    let jan = Month::parse("2026-01").unwrap();
    assert_eq!(m.status(7, jan).await.unwrap(), Status::Due);
    m.observe(
        9,
        "new",
        false,
        true,
        NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(m.status(9, jan).await.unwrap(), Status::NotRequired);
    m.observe(
        7,
        "old",
        false,
        false,
        NaiveDate::from_ymd_opt(2026, 2, 5).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(m.status(7, jan).await.unwrap(), Status::Due);
    assert_eq!(
        m.status(7, Month::parse("2026-03").unwrap()).await.unwrap(),
        Status::NotRequired
    );
}
#[tokio::test]
async fn concurrent_confirmation_and_ledger_overflow_are_safe() {
    let path = std::env::temp_dir().join(format!(
        "xecut-money-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let m = Money::new(pool, config()).unwrap();
    m.initialize().await.unwrap();
    let c = m
        .claim(-1, 1, 7, 1000, &[Month::parse("2026-01").unwrap()])
        .await
        .unwrap();
    m.attach_card(c.id, 99).await.unwrap();
    let (a, b) = tokio::join!(m.confirm(-1, 99, 42), m.confirm(-1, 99, 42));
    assert_ne!(a.unwrap(), b.unwrap());
    m.entry(-1, 2, 42, i64::MAX - 1000, "income", "opening donation")
        .await
        .unwrap();
    assert!(m.entry(-1, 3, 42, 1, "income", "overflow").await.is_err());
    assert_eq!(m.balance().await.unwrap(), i64::MAX);
    m.pool.close().await;
    std::fs::remove_file(path).unwrap();
}
#[tokio::test]
async fn graph_uses_posting_dates_and_opening_carry() {
    let m = ledger().await;
    m.entry(-1, 1, 42, 2000, "income", "donation")
        .await
        .unwrap();
    let end = Month::current();
    let points = m.graph(end, 12).await.unwrap();
    assert!(
        points
            .iter()
            .all(|(month, _)| *month >= Month::parse("2026-01").unwrap())
    );
    assert_eq!(points.last().unwrap().1, 2000);
    sqlx::query("UPDATE money_ledger SET posted=?")
        .bind(format!("{}T12:00:00Z", end.shift(-4).0))
        .execute(&m.pool)
        .await
        .unwrap();
    let window = m.graph(end, 3).await.unwrap();
    assert_eq!(window[0].1, 2000);
    assert!(window.iter().all(|(_, balance)| *balance == 2000));
}
#[test]
fn config_and_reaction_gates_fail_closed() {
    let mut c = config();
    c.financier_user_id = None;
    assert!(c.validate().is_err());
    c = config();
    c.minimum_monthly_euros = None;
    assert!(c.validate().is_err());
    c = config();
    c.initial_roster_complete = false;
    assert!(c.validate().is_err());
    assert!(reaction_approved(-1, -1, Some(42), 42, &[], &["👍"]));
    assert!(!reaction_approved(-2, -1, Some(42), 42, &[], &["👍"]));
    assert!(!reaction_approved(-1, -1, None, 42, &[], &["👍"]));
    assert!(!reaction_approved(-1, -1, Some(7), 42, &[], &["👍"]));
    assert!(!reaction_approved(-1, -1, Some(42), 42, &["👍"], &[]));
    assert!(!reaction_approved(-1, -1, Some(42), 42, &["👍"], &["👍"]));
    assert!(Month::parse("😀-10").is_err());
    assert!(parse_euros("92233720368547758.07").is_ok());
    assert!(parse_euros("92233720368547758.08").is_err());
}

#[tokio::test]
async fn month_status_and_minimum_policy_history_are_exact() {
    let m = ledger().await;
    let month = Month::parse("2026-01").unwrap();
    assert_eq!(
        m.status(7, month.shift(-1)).await.unwrap(),
        Status::NotRequired
    );
    let c = m.claim(-1, 1, 7, 499, &[month]).await.unwrap();
    m.attach_card(c.id, 9).await.unwrap();
    m.confirm(-1, 9, 42).await.unwrap();
    assert_eq!(m.status(7, month).await.unwrap(), Status::Partial);
    let c = m.claim(-1, 2, 7, 501, &[month]).await.unwrap();
    m.attach_card(c.id, 10).await.unwrap();
    m.confirm(-1, 10, 42).await.unwrap();
    assert_eq!(m.status(7, month).await.unwrap(), Status::Full);
    let mut changed = config();
    changed.minimum_monthly_euros = Some("20".into());
    assert!(
        Money::new(m.pool.clone(), changed.clone())
            .unwrap()
            .initialize()
            .await
            .is_err()
    );
    changed.minimum_effective_month = Some(Month::current().shift(1).to_string());
    Money::new(m.pool.clone(), changed)
        .unwrap()
        .initialize()
        .await
        .unwrap();
    assert_eq!(m.status(7, month).await.unwrap(), Status::Full);
    assert!(m.claim(-1, 3, 7, 100, &[month, month]).await.is_err());
}

#[tokio::test]
async fn income_expense_replay_and_reversal_preserve_audit() {
    let m = ledger().await;
    assert!(
        m.entry(-1, 1, 7, 100, "income", "unauthorized")
            .await
            .is_err()
    );
    let income = m
        .entry(-1, 1, 42, 1234, "income", "gift <script>")
        .await
        .unwrap();
    assert_eq!(
        m.entry(-1, 1, 42, 9999, "income", "edited update")
            .await
            .unwrap(),
        income
    );
    let expense = m
        .entry(-1, 2, 42, -234, "expense", "supplies")
        .await
        .unwrap();
    assert_eq!(m.balance().await.unwrap(), 1000);
    assert!(m.reverse(-1, 3, 7, expense, "unauthorized").await.is_err());
    let reverse = m
        .reverse(-1, 3, 42, expense, "wrong receipt")
        .await
        .unwrap();
    assert_eq!(
        m.reverse(-1, 3, 42, expense, "replay").await.unwrap(),
        reverse
    );
    assert!(
        m.reverse(-1, 4, 42, reverse, "reverse reversal")
            .await
            .is_err()
    );
    assert_eq!(m.balance().await.unwrap(), 1234);
    let audit = m.history(20).await.unwrap();
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[0].actor, 42);
    assert_eq!(audit[0].reverse_id, Some(expense));
    assert_eq!(audit[2].description, "gift <script>");
}

#[tokio::test]
async fn cash_near_utc_month_end_belongs_to_belgrade_month() {
    let money = ledger().await;
    money
        .entry(-1, 1, 42, 2000, "income", "late donation")
        .await
        .unwrap();
    sqlx::query("UPDATE money_ledger SET posted='2026-01-31T23:30:00Z'")
        .execute(&money.pool)
        .await
        .unwrap();
    let january = Month::parse("2026-01").unwrap();
    let february = Month::parse("2026-02").unwrap();
    assert_eq!(
        money.graph(february, 2).await.unwrap(),
        vec![(january, 0), (january, 0), (february, 2000)]
    );
}

#[tokio::test]
async fn departed_bots_do_not_trigger_incomplete_resident_roster() {
    let money = ledger().await;
    let date = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
    money
        .observe(9, "Chat bot", true, true, date)
        .await
        .unwrap();
    assert_eq!(money.known_active_count().await.unwrap(), 3);
    assert_eq!(
        money.status(9, Month::of(date)).await.unwrap(),
        Status::NotRequired
    );
    money
        .observe(9, "Chat bot", true, false, date)
        .await
        .unwrap();
    assert_eq!(money.known_active_count().await.unwrap(), 2);
}
