use crate::config::MoneyConfig;
use anyhow::{Result, bail, ensure};
use chrono::{Datelike, NaiveDate, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

pub fn parse_euros(text: &str) -> Result<i64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    ensure!(
        !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()),
        "amount must be a positive decimal EUR amount"
    );
    ensure!(
        fraction.len() <= 2
            && fraction.bytes().all(|b| b.is_ascii_digit())
            && (!text.contains('.') || !fraction.is_empty()),
        "use at most two decimal places"
    );
    let whole: i64 = whole
        .parse()
        .map_err(|_| anyhow::anyhow!("amount overflow"))?;
    let fraction: i64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i64>()? * if fraction.len() == 1 { 10 } else { 1 }
    };
    let amount = whole
        .checked_mul(100)
        .and_then(|n| n.checked_add(fraction))
        .ok_or_else(|| anyhow::anyhow!("amount overflow"))?;
    ensure!(amount > 0, "amount must be positive");
    Ok(amount)
}
pub fn euros(cents: i64) -> String {
    let n = cents.unsigned_abs();
    format!(
        "{}{}.{:02} EUR",
        if cents < 0 { "-" } else { "" },
        n / 100,
        n % 100
    )
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month(pub NaiveDate);
impl Month {
    pub fn parse(s: &str) -> Result<Self> {
        ensure!(
            s.is_ascii()
                && s.len() == 7
                && s.as_bytes()[4] == b'-'
                && s[..4].bytes().all(|b| b.is_ascii_digit())
                && s[5..].bytes().all(|b| b.is_ascii_digit()),
            "month must be YYYY-MM"
        );
        let date = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d")?;
        ensure!((1..=9998).contains(&date.year()), "month is out of range");
        Ok(Self(date))
    }
    pub fn current() -> Self {
        Self::of(crate::datetime::now().date_naive())
    }
    pub fn of(date: NaiveDate) -> Self {
        Self(date.with_day(1).unwrap())
    }
    pub fn shift(self, n: i32) -> Self {
        let index = self.0.year() * 12 + self.0.month0() as i32 + n;
        Self(
            NaiveDate::from_ymd_opt(index.div_euclid(12), index.rem_euclid(12) as u32 + 1, 1)
                .unwrap(),
        )
    }
}
impl std::fmt::Display for Month {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m"))
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Full,
    Partial,
    Pending,
    Due,
    NotRequired,
}
impl Status {
    pub fn emoji(&self) -> &'static str {
        match self {
            Self::Full => "✅",
            Self::Partial => "🟡",
            Self::Pending => "🕓",
            Self::Due => "❌",
            Self::NotRequired => "➖",
        }
    }
}
#[derive(Debug)]
pub struct Claim {
    pub id: i64,
    pub user_id: i64,
    pub amount: i64,
    pub card_id: Option<i32>,
    pub delivery_started: bool,
    pub confirmed: bool,
}
#[derive(Debug)]
pub struct Entry {
    pub id: i64,
    pub amount: i64,
    pub kind: String,
    pub description: String,
    pub actor: i64,
    pub posted: String,
    pub reverse_id: Option<i64>,
}
#[derive(Clone)]
pub struct Money {
    pub pool: SqlitePool,
    pub config: MoneyConfig,
    pub start: Month,
    pub financier: i64,
}
pub fn private_gate(chat: i64, private: i64) -> bool {
    chat == private
}
pub fn reaction_approved(
    chat: i64,
    private: i64,
    user: Option<u64>,
    financier: u64,
    old: &[&str],
    new: &[&str],
) -> bool {
    private_gate(chat, private)
        && user == Some(financier)
        && !old.contains(&"👍")
        && new.contains(&"👍")
}
impl Money {
    pub fn new(pool: SqlitePool, config: MoneyConfig) -> Result<Self> {
        config.validate()?;
        ensure!(config.enabled, "finance disabled");
        let start = Month::parse(config.start_month.as_deref().unwrap())?;
        let financier = config.financier_user_id.unwrap() as i64;
        Ok(Self {
            pool,
            config,
            start,
            financier,
        })
    }
    pub async fn initialize(&self) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT OR IGNORE INTO money_settings(id,start_month) VALUES(1,?)")
            .bind(self.start.to_string())
            .execute(&mut *tx)
            .await?;
        let start: String = sqlx::query_scalar("SELECT start_month FROM money_settings WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            start == self.start.to_string(),
            "accounting start is immutable after initialization"
        );
        let month = Month::parse(
            self.config
                .minimum_effective_month
                .as_deref()
                .unwrap_or(&start),
        )?;
        let minimum = parse_euros(self.config.minimum_monthly_euros.as_deref().unwrap())?;
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT minimum FROM money_policies WHERE month=?")
                .bind(month.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(old) = existing {
            ensure!(
                old == minimum,
                "minimum policy is immutable; set a new minimum_effective_month instead"
            );
        } else {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM money_policies")
                .fetch_one(&mut *tx)
                .await?;
            ensure!(
                count == 0 || month >= Month::current(),
                "new minimum policy must take effect this month or later"
            );
            ensure!(
                count != 0 || month == self.start,
                "first minimum policy must begin at accounting start"
            );
            sqlx::query("INSERT INTO money_policies(month,minimum) VALUES(?,?)")
                .bind(month.to_string())
                .bind(minimum)
                .execute(&mut *tx)
                .await?;
        }
        for &id in &self.config.initial_member_ids {
            let added = sqlx::query(
                "INSERT OR IGNORE INTO money_members(user_id,name,is_bot) VALUES(?,?,0)",
            )
            .bind(id as i64)
            .bind(format!("ID {id}"))
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if added > 0 {
                sqlx::query("INSERT INTO money_memberships(user_id,joined) VALUES(?,?)")
                    .bind(id as i64)
                    .bind(self.start.0.to_string())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn observe(
        &self,
        id: i64,
        name: &str,
        is_bot: bool,
        active: bool,
        date: NaiveDate,
    ) -> Result<()> {
        ensure!(id > 0, "invalid member ID");
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO money_members(user_id,name,is_bot) VALUES(?,?,?) ON CONFLICT(user_id) DO UPDATE SET name=excluded.name,is_bot=excluded.is_bot").bind(id).bind(name).bind(is_bot).execute(&mut *tx).await?;
        if active {
            sqlx::query("INSERT INTO money_memberships(user_id,joined) SELECT ?,? WHERE NOT EXISTS(SELECT 1 FROM money_memberships WHERE user_id=? AND departed IS NULL)").bind(id).bind(date.max(self.start.0).to_string()).bind(id).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE money_memberships SET departed=MAX(joined,?) WHERE user_id=? AND departed IS NULL").bind(date.to_string()).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn members(&self) -> Result<Vec<(i64, String)>> {
        Ok(sqlx::query(
            "SELECT user_id,name FROM money_members WHERE is_bot=0 ORDER BY name,user_id",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
    }
    pub async fn known_active_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM money_members m WHERE EXISTS(SELECT 1 FROM money_memberships s WHERE s.user_id=m.user_id AND s.departed IS NULL)").fetch_one(&self.pool).await?)
    }
    pub async fn claim(
        &self,
        chat: i64,
        source: i32,
        user: i64,
        amount: i64,
        months: &[Month],
    ) -> Result<Claim> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing=sqlx::query("SELECT id,user_id,amount,card_id,delivery_started,confirmed FROM money_claims WHERE chat_id=? AND source_id=?")
            .bind(chat).bind(source).fetch_optional(&mut *tx).await?;
        if let Some(row) = existing {
            let claim = claim_row(&row);
            ensure!(claim.user_id == user, "claim belongs to another resident");
            tx.commit().await?;
            return Ok(claim);
        }
        ensure!(
            amount > 0 && !months.is_empty() && months.len() <= 36 && amount >= months.len() as i64,
            "allocate positive cents across 1..36 unique months"
        );
        for (i, month) in months.iter().enumerate() {
            ensure!(
                *month >= self.start && *month <= Month::current().shift(36),
                "payment month outside accounting range"
            );
            ensure!(!months[..i].contains(month), "duplicate month");
        }
        let row=sqlx::query("INSERT INTO money_claims(chat_id,source_id,user_id,amount) VALUES(?,?,?,?) RETURNING id,user_id,amount,card_id,delivery_started,confirmed")
            .bind(chat).bind(source).bind(user).bind(amount).fetch_one(&mut *tx).await?;
        let claim = claim_row(&row);
        for (i, month) in months.iter().enumerate() {
            let cents =
                amount / months.len() as i64 + i64::from((i as i64) < amount % months.len() as i64);
            sqlx::query("INSERT INTO money_allocations(claim_id,month,amount) VALUES(?,?,?)")
                .bind(claim.id)
                .bind(month.to_string())
                .bind(cents)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(claim)
    }
    pub async fn allocations(&self, id: i64) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query(
            "SELECT month,amount FROM money_allocations WHERE claim_id=? ORDER BY month",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
    }
    pub async fn find_claim(&self, id: i64, chat: i64) -> Result<Claim> {
        let row=sqlx::query("SELECT id,user_id,amount,card_id,delivery_started,confirmed FROM money_claims WHERE id=? AND chat_id=?")
            .bind(id).bind(chat).fetch_one(&self.pool).await?;
        Ok(claim_row(&row))
    }
    pub async fn reserve_card(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("UPDATE money_claims SET delivery_started=1 WHERE id=? AND delivery_started=0 AND card_id IS NULL").bind(id).execute(&self.pool).await?.rows_affected()>0)
    }
    pub async fn attach_card(&self, id: i64, card: i32) -> Result<()> {
        ensure!(sqlx::query("UPDATE money_claims SET card_id=?,delivery_started=1 WHERE id=? AND (card_id IS NULL OR card_id=?)").bind(card).bind(id).bind(card).execute(&self.pool).await?.rows_affected()==1,"claim not found or card already attached");
        Ok(())
    }
    pub async fn confirm(&self, chat: i64, card: i32, actor: i64) -> Result<bool> {
        if actor != self.financier {
            return Ok(false);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row=sqlx::query("SELECT id,user_id,amount,card_id,delivery_started,confirmed FROM money_claims WHERE chat_id=? AND card_id=?").bind(chat).bind(card).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(false) };
        let c = claim_row(&row);
        if c.confirmed {
            return Ok(false);
        };
        checked_balance(&mut tx, c.amount).await?;
        sqlx::query("INSERT INTO money_ledger(amount,kind,description,actor,posted,claim_id) VALUES(?,'payment',?,?,?,?)").bind(c.amount).bind(format!("Contribution from ID {}",c.user_id)).bind(actor).bind(Utc::now().to_rfc3339()).bind(c.id).execute(&mut *tx).await?;
        sqlx::query("UPDATE money_claims SET confirmed=1 WHERE id=?")
            .bind(c.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn receipts(&self) -> Result<Vec<(i64, i64, i32)>> {
        Ok(sqlx::query("SELECT id,chat_id,card_id FROM money_claims WHERE confirmed=1 AND acknowledged=0 AND card_id IS NOT NULL").fetch_all(&self.pool).await?.into_iter().map(|r|(r.get(0),r.get(1),r.get(2))).collect())
    }
    pub async fn acknowledged(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE money_claims SET acknowledged=1 WHERE id=? AND confirmed=1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn balance(&self) -> Result<i64> {
        sum(
            sqlx::query_scalar::<_, i64>("SELECT amount FROM money_ledger ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn entry(
        &self,
        chat: i64,
        source: i32,
        actor: i64,
        amount: i64,
        kind: &str,
        description: &str,
    ) -> Result<i64> {
        ensure!(
            actor == self.financier,
            "only the numeric financier may post entries"
        );
        ensure!(
            !description.trim().is_empty() && description.len() <= 1000,
            "description must contain 1..1000 bytes"
        );
        ensure!(
            (kind == "income" && amount > 0) || (kind == "expense" && amount < 0),
            "invalid ledger entry"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(id) =
            sqlx::query_scalar("SELECT id FROM money_ledger WHERE chat_id=? AND source_id=?")
                .bind(chat)
                .bind(source)
                .fetch_optional(&mut *tx)
                .await?
        {
            return Ok(id);
        };
        checked_balance(&mut tx, amount).await?;
        let id=sqlx::query("INSERT INTO money_ledger(amount,kind,description,actor,posted,chat_id,source_id) VALUES(?,?,?,?,?,?,?)").bind(amount).bind(kind).bind(description).bind(actor).bind(Utc::now().to_rfc3339()).bind(chat).bind(source).execute(&mut *tx).await?.last_insert_rowid();
        tx.commit().await?;
        Ok(id)
    }
    pub async fn reverse(
        &self,
        chat: i64,
        source: i32,
        actor: i64,
        id: i64,
        reason: &str,
    ) -> Result<i64> {
        ensure!(
            actor == self.financier,
            "only the numeric financier may reverse entries"
        );
        ensure!(
            !reason.trim().is_empty() && reason.len() <= 1000,
            "reason must contain 1..1000 bytes"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            sqlx::query_scalar("SELECT id FROM money_ledger WHERE chat_id=? AND source_id=?")
                .bind(chat)
                .bind(source)
                .fetch_optional(&mut *tx)
                .await?
        {
            return Ok(existing);
        };
        let row=sqlx::query("SELECT amount,kind FROM money_ledger WHERE id=? AND NOT EXISTS(SELECT 1 FROM money_ledger WHERE reverse_id=?)").bind(id).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow::anyhow!("entry missing or already reversed"))?;
        ensure!(
            row.get::<String, _>(1) != "reversal",
            "cannot reverse a reversal"
        );
        let amount = row
            .get::<i64, _>(0)
            .checked_neg()
            .ok_or_else(|| anyhow::anyhow!("reversal overflow"))?;
        checked_balance(&mut tx, amount).await?;
        let result=sqlx::query("INSERT INTO money_ledger(amount,kind,description,actor,posted,reverse_id,chat_id,source_id) VALUES(?,'reversal',?,?,?,?,?,?)").bind(amount).bind(reason).bind(actor).bind(Utc::now().to_rfc3339()).bind(id).bind(chat).bind(source).execute(&mut *tx).await?.last_insert_rowid();
        tx.commit().await?;
        Ok(result)
    }
    pub async fn history(&self, limit: usize) -> Result<Vec<Entry>> {
        ensure!((1..=100).contains(&limit), "history limit must be 1..100");
        Ok(sqlx::query("SELECT id,amount,kind,description,actor,posted,reverse_id FROM money_ledger ORDER BY id DESC LIMIT ?").bind(limit as i64).fetch_all(&self.pool).await?.into_iter().map(|r|Entry{id:r.get(0),amount:r.get(1),kind:r.get(2),description:r.get(3),actor:r.get(4),posted:r.get(5),reverse_id:r.get(6)}).collect())
    }
    pub async fn paid_cents(&self, user: i64, month: Month) -> Result<i64> {
        sum(sqlx::query_scalar("SELECT a.amount FROM money_allocations a JOIN money_claims c ON c.id=a.claim_id JOIN money_ledger l ON l.claim_id=c.id WHERE c.user_id=? AND a.month=? AND NOT EXISTS(SELECT 1 FROM money_ledger r WHERE r.reverse_id=l.id)").bind(user).bind(month.to_string()).fetch_all(&self.pool).await?)
    }
    pub async fn status(&self, user: i64, month: Month) -> Result<Status> {
        if month < self.start {
            return Ok(Status::NotRequired);
        };
        let owed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM money_memberships s JOIN money_members m ON m.user_id=s.user_id WHERE s.user_id=? AND m.is_bot=0 AND s.joined<? AND (s.departed IS NULL OR s.departed>=?))").bind(user).bind(month.shift(1).0.to_string()).bind(month.0.to_string()).fetch_one(&self.pool).await?;
        if !owed {
            return Ok(Status::NotRequired);
        };
        let minimum: Option<i64> = sqlx::query_scalar(
            "SELECT minimum FROM money_policies WHERE month<=? ORDER BY month DESC LIMIT 1",
        )
        .bind(month.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(minimum) = minimum else {
            bail!("no minimum policy for {month}")
        };
        let paid = self.paid_cents(user, month).await?;
        if paid >= minimum {
            return Ok(Status::Full);
        };
        if paid > 0 {
            return Ok(Status::Partial);
        };
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM money_claims c JOIN money_allocations a ON a.claim_id=c.id WHERE c.user_id=? AND a.month=? AND c.confirmed=0)").bind(user).bind(month.to_string()).fetch_one(&self.pool).await?;
        Ok(if pending {
            Status::Pending
        } else {
            Status::Due
        })
    }
    pub async fn graph(&self, end: Month, months: usize) -> Result<Vec<(Month, i64)>> {
        ensure!((1..=36).contains(&months), "months must be 1..36");
        let first = end.shift(1 - months as i32).max(self.start);
        ensure!(first <= end, "accounting has not started yet");
        let mut rows = sqlx::query(
            "SELECT amount,posted FROM money_ledger WHERE substr(posted,1,7)<=? ORDER BY posted,id",
        )
        .bind(end.to_string())
        .fetch(&self.pool);
        let mut balance = 0i64;
        let mut result = vec![(first, balance)];
        let mut month = first;
        while let Some(row) = futures::TryStreamExt::try_next(&mut rows).await? {
            let posted = chrono::DateTime::parse_from_rfc3339(row.get("posted"))?;
            let posting_month = Month::of(
                posted
                    .with_timezone(&chrono_tz::Europe::Belgrade)
                    .date_naive(),
            );
            if posting_month > end {
                break;
            }
            while month < posting_month {
                result.push((month, balance));
                month = month.shift(1);
            }
            balance = balance
                .checked_add(row.get("amount"))
                .ok_or_else(|| anyhow::anyhow!("balance overflow"))?;
            if posting_month < first {
                result[0].1 = balance;
            }
        }
        while month <= end {
            result.push((month, balance));
            month = month.shift(1);
        }
        Ok(result)
    }
}
fn claim_row(r: &sqlx::sqlite::SqliteRow) -> Claim {
    Claim {
        id: r.get(0),
        user_id: r.get(1),
        amount: r.get(2),
        card_id: r.get(3),
        delivery_started: r.get(4),
        confirmed: r.get(5),
    }
}
fn sum(values: Vec<i64>) -> Result<i64> {
    values.into_iter().try_fold(0i64, |a, b| {
        a.checked_add(b)
            .ok_or_else(|| anyhow::anyhow!("aggregate overflow"))
    })
}
async fn checked_balance(tx: &mut Transaction<'_, Sqlite>, delta: i64) -> Result<()> {
    let balance = sum(
        sqlx::query_scalar("SELECT amount FROM money_ledger ORDER BY id")
            .fetch_all(&mut **tx)
            .await?,
    )?;
    ensure!(balance.checked_add(delta).is_some(), "balance overflow");
    Ok(())
}

/// Native RGB chart with embedded bitmap lettering; no system fonts or subprocesses.
pub fn chart_png(points: &[(Month, i64)]) -> Result<Vec<u8>> {
    use font8x8::UnicodeFonts;
    ensure!(points.len() >= 2, "chart requires accounting data");
    const W: usize = 1100;
    const H: usize = 520;
    let mut pixels = vec![248u8; W * H * 3];
    fn pixel(p: &mut [u8], x: i32, y: i32, c: [u8; 3]) {
        if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
            let at = (y as usize * W + x as usize) * 3;
            p[at..at + 3].copy_from_slice(&c);
        }
    }
    fn text(p: &mut [u8], x: i32, y: i32, s: &str, scale: i32) {
        for (i, ch) in s.chars().enumerate() {
            if let Some(glyph) = font8x8::BASIC_FONTS.get(ch) {
                for (dy, row) in glyph.iter().enumerate() {
                    for dx in 0..8 {
                        if row & (1 << dx) != 0 {
                            for sy in 0..scale {
                                for sx in 0..scale {
                                    pixel(
                                        p,
                                        x + i as i32 * 8 * scale + dx * scale + sx,
                                        y + dy as i32 * scale + sy,
                                        [35, 45, 60],
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    fn line(p: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        for i in 0..=steps {
            pixel(p, x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps, c);
        }
    }
    text(
        &mut pixels,
        24,
        18,
        "EUR wallet - monthly closing balance",
        2,
    );
    let summary = format!(
        "Opening {} | Closing {}",
        euros(points[0].1),
        euros(points.last().unwrap().1)
    );
    text(
        &mut pixels,
        24,
        48,
        &summary,
        if summary.len() > 65 { 1 } else { 2 },
    );
    let min = points.iter().map(|p| p.1).min().unwrap().min(0) as f64;
    let max = (points.iter().map(|p| p.1).max().unwrap().max(0) as f64).max(min + 100.0);
    let span = max - min;
    let left = 230;
    let right = 1070;
    let top = 110;
    let bottom = 425;
    for i in 0..=4 {
        let y = top + (bottom - top) * i / 4;
        let value = max - span * i as f64 / 4.0;
        let label = format!("{:.2}", value / 100.0);
        text(
            &mut pixels,
            8,
            y - 8,
            &label,
            if label.len() > 13 { 1 } else { 2 },
        );
        line(&mut pixels, left, y, right, y, [220, 225, 230]);
    }
    let stride = (points.len() - 1).div_ceil(6).max(1);
    let mut previous = None;
    for (i, (month, value)) in points.iter().enumerate() {
        let x = left + (right - left) * i as i32 / (points.len() - 1) as i32;
        let y = bottom - ((*value as f64 - min) / span * (bottom - top) as f64) as i32;
        if let Some((px, py)) = previous {
            for dy in -1..=1 {
                line(&mut pixels, px, py + dy, x, y + dy, [0, 120, 95]);
            }
        }
        previous = Some((x, y));
        if i > 0 && ((i - 1).is_multiple_of(stride) || i == points.len() - 1) {
            text(
                &mut pixels,
                (x - 42).clamp(left, right - 85),
                bottom + 24 + (if i == points.len() - 1 { 18 } else { 0 }),
                &month.to_string(),
                1,
            );
        }
    }
    text(
        &mut pixels,
        24,
        492,
        "Opening carry-in is included. Contribution target months do not move cash dates.",
        1,
    );
    let mut result = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut result, W as u32, H as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&pixels)?;
    }
    Ok(result)
}
