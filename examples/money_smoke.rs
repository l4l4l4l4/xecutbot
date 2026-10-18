//! Credential-free dispatcher launcher for `make smoke`; loopback transport only.
use anyhow::{Context as _, Result, ensure};
use chrono::NaiveDate;
use sqlx::SqlitePool;
use std::{ffi::OsStr, net::SocketAddr, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;
use xecut_bot::{
    Config, TelegramBot, Visit, Visits,
    backend::{Backend, Uid},
};

struct SmokeBackend {
    pool: SqlitePool,
    visits: Visits,
}

impl Backend for SmokeBackend {
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    async fn check_in(&self, _: Uid, _: Option<String>) -> Result<()> {
        anyhow::bail!("unexpected visit mutation in money smoke")
    }
    async fn check_out(&self, _: Uid) -> Result<()> {
        anyhow::bail!("unexpected visit mutation in money smoke")
    }
    async fn plan_visit(&self, _: Uid, _: NaiveDate, _: Option<String>) -> Result<()> {
        anyhow::bail!("unexpected visit mutation in money smoke")
    }
    async fn unplan_visit(&self, _: Uid, _: NaiveDate) -> Result<()> {
        anyhow::bail!("unexpected visit mutation in money smoke")
    }
    async fn check_out_everybody(&self) -> Result<()> {
        anyhow::bail!("unexpected visit mutation in money smoke")
    }
    async fn get_visits(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<Visit>> {
        self.visits.get_visits(from, to).await
    }
    async fn update(&self) -> Result<bool> {
        anyhow::bail!("self-update is forbidden in money smoke")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 3, "usage: money_smoke CONFIG LOOPBACK_SOCKET");
    let address: SocketAddr = args[2].parse()?;
    ensure!(address.ip().is_loopback(), "only a loopback API is allowed");
    let directory = Path::new(&args[1])
        .parent()
        .context("fixture config needs a parent directory")?
        .canonicalize()?;
    ensure!(
        directory.starts_with(std::env::temp_dir().canonicalize()?)
            && directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("xecutbot-money-smoke-")),
        "config must live in the generated temporary smoke directory"
    );
    let mut config = Config::new("xecut_money_smoke", vec![args[1].clone().into()])?;
    let database = config
        .db
        .sqlite_path
        .strip_prefix("sqlite://")
        .and_then(|path| path.strip_suffix("?mode=rwc"))
        .map(Path::new);
    ensure!(
        database.is_some_and(|path| path.parent() == Some(directory.as_path())
            && path.file_name() == Some(OsStr::new("ledger.sqlite"))),
        "only the generated temporary ledger is allowed"
    );
    // Never use a real token, even if one is accidentally present in the fixture config.
    config.telegram_bot.bot_token = "303:verification".into();
    config.telegram_bot.money.validate()?;
    let pool = SqlitePool::connect(&config.db.sqlite_path).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    let backend = Arc::new(SmokeBackend {
        visits: Visits::new(pool.clone())?,
        pool,
    });
    let mut bot = TelegramBot::new(config.telegram_bot, Arc::downgrade(&backend))?;
    Arc::get_mut(&mut bot).expect("unique smoke bot").bot =
        teloxide::Bot::new("303:verification").set_api_url(format!("http://{address}/").parse()?);
    let cancellation = CancellationToken::new();
    let mut running = tokio::spawn(bot.run(cancellation.clone()));
    tokio::select! {
        result = &mut running => { result??; }
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancellation.cancel();
            running.await??;
        }
    }
    backend.pool.close().await;
    Ok(())
}
