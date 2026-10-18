use std::sync::atomic::{AtomicBool, Ordering};
use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::SqlitePool;
use teloxide::types::UserId;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::bot::TelegramBot;
use crate::config::{BackendConfig, DbConfig};
use crate::datetime::today_abstract;
use crate::rest_api::RestApi;
use crate::visits::VisitUpdate;
use crate::{Config, Visit, VisitStatus, Visits};

pub struct BackendImpl {
    pub pool: SqlitePool,
    pub visits: Visits,
    pub tg_bot: Arc<TelegramBot<Self>>,
    pub rest_api: RestApi<Self>,
    pub config: BackendConfig,
    ct: CancellationToken,
    should_reexec: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uid(pub UserId);

impl From<i64> for Uid {
    fn from(value: i64) -> Self {
        Uid(UserId(value as u64))
    }
}

impl From<Uid> for i64 {
    fn from(val: Uid) -> Self {
        val.0.0 as i64
    }
}

pub trait Backend: Sized + Send + Sync + 'static {
    fn pool(&self) -> &SqlitePool;

    fn check_in(
        &self,
        person: Uid,
        purpose: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;
    fn check_out(&self, person: Uid) -> impl Future<Output = Result<()>> + Send;
    fn plan_visit(
        &self,
        person: Uid,
        day: NaiveDate,
        purpose: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;
    fn unplan_visit(&self, person: Uid, day: NaiveDate) -> impl Future<Output = Result<()>> + Send;
    fn check_out_everybody(&self) -> impl Future<Output = Result<()>> + Send;
    fn get_visits(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> impl Future<Output = Result<Vec<Visit>>> + Send;

    fn update(&self) -> impl Future<Output = Result<bool>> + Send;
}

fn maybe_panic(text: &str) -> Result<()> {
    match text {
        "panic" => panic!("ayaya"),
        "error" => anyhow::bail!("ayayaya"),
        _ => Ok(()),
    }
}

impl Backend for BackendImpl {
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn check_in(&self, person: Uid, purpose: Option<String>) -> Result<()> {
        let visit_update = VisitUpdate {
            person,
            day: today_abstract(),
            purpose,
            status: VisitStatus::CheckedIn,
        };

        let updated = self.visits.upsert_visit(&visit_update).await?;

        if updated {
            self.tg_bot.announce_check_in(&visit_update).await?;
        }

        Ok(())
    }

    async fn check_out(&self, person: Uid) -> Result<()> {
        let visit_update = VisitUpdate {
            person,
            day: today_abstract(),
            purpose: None,
            status: VisitStatus::CheckedOut,
        };

        self.visits.upsert_visit(&visit_update).await?;

        Ok(())
    }

    async fn plan_visit(&self, person: Uid, day: NaiveDate, purpose: Option<String>) -> Result<()> {
        let visit_update = VisitUpdate {
            person,
            day,
            purpose,
            status: VisitStatus::Planned,
        };

        let updated = self.visits.upsert_visit(&visit_update).await?;

        if updated {
            self.tg_bot.announce_plan(&visit_update).await?;
        }

        maybe_panic(visit_update.purpose.as_deref().unwrap_or_default())?;

        Ok(())
    }

    async fn unplan_visit(&self, person: Uid, day: NaiveDate) -> Result<()> {
        let deleted = self.visits.delete_visit(person, day).await?;

        if deleted {
            self.tg_bot.announce_unplan(person, day).await?;
        }

        Ok(())
    }

    async fn check_out_everybody(&self) -> Result<()> {
        self.visits.check_out_everybody(today_abstract()).await?;
        Ok(())
    }

    async fn get_visits(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<Visit>> {
        self.visits.get_visits(from, to).await
    }

    async fn update(&self) -> Result<bool> {
        if !self.config.enable_update {
            return Ok(false);
        }

        crate::selfupdate::update().await?;

        self.should_reexec.store(true, Ordering::Relaxed);
        self.ct.cancel();

        Ok(true)
    }
}

pub async fn connect_db(db_config: &DbConfig) -> Result<SqlitePool> {
    Ok(SqlitePool::connect(&db_config.sqlite_path).await?)
}

impl BackendImpl {
    pub async fn new(config_files: Vec<PathBuf>) -> Result<Arc<Self>> {
        let config = Config::new("xecut_bot", config_files)?;
        if let Some(announcement) = config
            .telegram_bot
            .decentrala
            .as_ref()
            .filter(|config| config.enabled)
        {
            announcement.validate()?;
        }

        let pool = connect_db(&config.db).await?;

        let visits = Visits::new(pool.clone())?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        let backend = Arc::new_cyclic(|backend| BackendImpl {
            pool,
            visits,
            tg_bot: TelegramBot::new(config.telegram_bot, backend.clone()).unwrap(),
            rest_api: RestApi::new(config.rest_api, backend.clone()),
            config: config.backend,
            ct: CancellationToken::new(),
            should_reexec: false.into(),
        });

        Ok(backend)
    }

    pub async fn run(self: Arc<Self>) -> Result<bool> {
        let ct = self.ct.clone();

        let mut js = JoinSet::new();

        js.spawn(self.visits.clone().run(ct.clone()));
        js.spawn(self.tg_bot.clone().run(ct.clone()));
        js.spawn(self.rest_api.clone().run(ct.clone()));

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            ct.cancel();
        });

        while let Some(r) = js.join_next().await {
            let _ = r?;
        }

        Ok(self.should_reexec.load(Ordering::Relaxed))
    }
}
