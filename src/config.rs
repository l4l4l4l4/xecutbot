use std::path::PathBuf;

use serde_derive::Deserialize;
use teloxide::types::ChatId;

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramBotConfig {
    pub bot_token: String,
    pub public_chat_id: ChatId,
    pub private_chat_id: ChatId,
    pub public_channel_id: ChatId,
    pub alert_chat_id: ChatId,
    #[serde(default)]
    pub money: MoneyConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MoneyConfig {
    pub enabled: bool,
    pub minimum_monthly_euros: Option<String>,
    pub financier_user_id: Option<u64>,
    pub start_month: Option<String>,
    pub minimum_effective_month: Option<String>,
    pub initial_member_ids: Vec<u64>,
    pub initial_roster_complete: bool,
    pub chart_months: usize,
    pub contact: String,
}

impl Default for MoneyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_monthly_euros: None,
            financier_user_id: None,
            start_month: None,
            minimum_effective_month: None,
            initial_member_ids: Vec::new(),
            initial_roster_complete: false,
            chart_months: 12,
            contact: "@rvbar_el3".into(),
        }
    }
}

impl MoneyConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        anyhow::ensure!(
            self.financier_user_id
                .is_some_and(|id| id > 0 && id <= i64::MAX as u64),
            "money.financier_user_id must be a positive numeric Telegram user ID; usernames do not authorize payments"
        );
        crate::money::parse_euros(
            self.minimum_monthly_euros
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("set money.minimum_monthly_euros explicitly"))?,
        )?;
        let start = crate::money::Month::parse(
            self.start_month
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("set money.start_month (YYYY-MM)"))?,
        )?;
        if let Some(month) = &self.minimum_effective_month {
            anyhow::ensure!(
                crate::money::Month::parse(month)? >= start,
                "minimum policy cannot precede accounting start"
            );
        }
        anyhow::ensure!(
            self.initial_roster_complete && !self.initial_member_ids.is_empty(),
            "import all human private-chat member IDs into money.initial_member_ids and set initial_roster_complete only after checking the full roster"
        );
        anyhow::ensure!(
            self.initial_member_ids
                .iter()
                .all(|id| *id > 0 && *id <= i64::MAX as u64),
            "initial_member_ids must contain positive numeric IDs"
        );
        anyhow::ensure!(
            (1..=36).contains(&self.chart_months),
            "chart_months must be 1..36"
        );
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DbConfig {
    pub sqlite_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RestApiConfig {
    pub bind_address: String,
}

impl Default for RestApiConfig {
    fn default() -> Self {
        Self {
            bind_address: "127.0.0.1:3000".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct BackendConfig {
    pub enable_update: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub telegram_bot: TelegramBotConfig,
    pub db: DbConfig,
    #[serde(default)]
    pub rest_api: RestApiConfig,
    #[serde(default)]
    pub backend: BackendConfig,
}

impl Config {
    pub fn new(env_prefix: &str, config_files: Vec<PathBuf>) -> Result<Self, config::ConfigError> {
        let mut builder = config::Config::builder();
        for config_file in config_files {
            builder = builder.add_source(config::File::from(config_file));
        }
        builder
            .add_source(config::Environment::with_prefix(env_prefix))
            .build()?
            .try_deserialize()
    }
}
