mod announcements;
mod livestatus;
mod money;
mod person_details;
mod status;
mod util;
mod visits;

use anyhow::Result;
use futures::FutureExt;
use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, RwLock, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

use teloxide::{
    Bot,
    dispatching::UpdateFilterExt as _,
    dptree,
    prelude::{Dispatcher, Requester as _},
    types::{CallbackQuery, Message, MessageId, Update},
    utils::command::BotCommands,
};

use crate::backend::Uid;
use crate::{backend::Backend, config::TelegramBotConfig};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(
        description = "📮 Репостнуть пост в live канал (реплайни на пост, доступно только резидентам)"
    )]
    PostLive,
    #[command(description = "ℹ️ Посмотреть что сейчас происходит в хакспейсе")]
    Status,
    #[command(description = "🗓️ Посмотреть кто собирается в хакспейс в ближайшие дни")]
    GetVisits,
    #[command(
        description = "🗓️ Запланировать зайти в хакспейс (опционально дата в формате YYYY-MM-DD и описание зачем)"
    )]
    PlanVisit,
    #[command(
        description = "🤔 Передумать заходить в хакспейс (опционально дата в формате YYYY-MM-DD)"
    )]
    UnplanVisit,
    #[command(description = "👷 Отметиться как зашедший (опционально описание зачем)")]
    CheckIn,
    #[command(description = "🌆 Отметиться как ушедший")]
    CheckOut,
    #[command(description = "🌒 Закрыть хакспейс")]
    Close,
    #[command(description = "🔃 Сделать закреп с текущей информацией о спейсе")]
    LiveStatus,
    #[command(description = "🧟 Убрать закреп с текущей информацией о спейсе")]
    UnLiveStatus,
    #[command(description = "💶 EUR ledger (use /help money)")]
    Money(String),
    #[command(description = "📖 Command help (optional: money)")]
    Help(String),
    #[command(hide)]
    Update,
}

pub struct TelegramBot<B: Backend> {
    config: TelegramBotConfig,
    pub bot: Bot,
    status_message_id: RwLock<Option<MessageId>>,
    backend: Weak<B>,
    money: std::sync::OnceLock<crate::money::Money>,
    money_card_lock: tokio::sync::Mutex<()>,
}

impl<B: Backend> TelegramBot<B> {
    pub fn new(config: TelegramBotConfig, backend: Weak<B>) -> Result<Arc<Self>> {
        config.money.validate()?;
        let bot = Bot::new(config.bot_token.clone());
        Ok(Arc::new(TelegramBot {
            config,
            bot,
            status_message_id: RwLock::new(None),
            money: std::sync::OnceLock::new(),
            money_card_lock: tokio::sync::Mutex::new(()),
            backend,
        }))
    }

    // Can not panic during execution of run(), because Backend is already constructed and not destructed yet
    fn backend(&self) -> Arc<B> {
        self.backend.upgrade().expect("Backend to be available")
    }

    async fn send_alert(&self) -> Result<()> {
        self.bot
            .send_message(self.config.alert_chat_id, "💥 Что-то пошло не так")
            .await?;
        Ok(())
    }

    pub async fn run(self: Arc<Self>, ct: CancellationToken) -> Result<()> {
        self.bot.set_my_commands(Command::bot_commands()).await?;
        self.initialize_money().await?;

        *self.status_message_id.write().unwrap() =
            livestatus::load_status_message_id(self.backend().pool()).await?;

        let handler = dptree::entry()
            .inspect(|u: Update| {
                log::trace!("Got update:\n{u:#?}");
            })
            .branch(
                Update::filter_message()
                    .inspect_async(Self::observe_money_message)
                    .branch(
                        dptree::entry()
                            .filter_map(|msg: Message, me: teloxide::types::Me| {
                                let text = msg.text()?;
                                money::parse_command(text, me.username())
                            })
                            .endpoint(Self::handle_message),
                    ),
            )
            .branch(Update::filter_chat_member().endpoint(Self::handle_money_member))
            .branch(Update::filter_message_reaction_updated().endpoint(Self::handle_money_reaction))
            .branch(Update::filter_callback_query().endpoint(Self::handle_callback_outer));

        let mut dispatcher = Dispatcher::builder(self.bot.clone(), handler)
            .dependencies(dptree::deps![self.clone()])
            .build();

        let token = dispatcher.shutdown_token();
        let ct1 = ct.clone();
        tokio::spawn(async move {
            ct1.cancelled().await;
            while token.shutdown().is_err() {
                log::warn!("Dispatcher not running yet, retrying to shut down");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            log::info!("Shutting down dispatcher");
        });

        let (_, _, receipt_result) = tokio::try_join!(
            tokio::spawn(async move {
                log::info!("Started dispatcher");
                dispatcher.dispatch().await
            }),
            tokio::spawn(self.clone().update_live_task(ct.clone())),
            tokio::spawn(self.clone().money_receipt_task(ct))
        )?;
        receipt_result?;

        Ok(())
    }

    async fn handle_message(self: Arc<Self>, msg: Message, cmd: Command) -> Result<()> {
        let res = AssertUnwindSafe(async {
            match cmd {
                Command::PostLive => self.handle_post_live(&msg).await,
                Command::Status => self.handle_status(&msg).await,
                Command::GetVisits => self.handle_get_visits(&msg).await,
                Command::PlanVisit => self.handle_plan_visit(&msg).await,
                Command::UnplanVisit => self.handle_unplan_visit(&msg).await,
                Command::CheckIn => self.handle_check_in(&msg).await,
                Command::CheckOut => self.handle_check_out(&msg).await,
                Command::Close => self.handle_close(&msg).await,
                Command::LiveStatus => self.handle_live_status(&msg).await,
                Command::UnLiveStatus => self.handle_unlive_status(&msg).await,
                Command::Update => self.handle_update(&msg).await,
                Command::Money(args) => self.handle_money(&msg, &args).await,
                Command::Help(args) => self.handle_help(&msg, &args).await,
            }
        })
        .catch_unwind()
        .await;
        if matches!(res, Err(_) | Ok(Err(_))) {
            self.send_alert().await?;
            self.send_message_reply(&msg, "😬 Что-то пошло не так, но админ уже об этом знает")
                .await?;
            if let Ok(e) = res {
                return e;
            }
        }
        Ok(())
    }

    async fn handle_update(&self, msg: &Message) -> Result<()> {
        if !self.check_author_is_resident(msg).await? {
            return Ok(());
        }

        if !self.backend().update().await? {
            self.send_message_reply(msg, "❌ Обновления выключены")
                .await?;
        } else {
            self.acknowledge_message(msg).await?;
        }

        Ok(())
    }

    async fn handle_callback_outer(self: Arc<Self>, q: CallbackQuery) -> Result<()> {
        let res = AssertUnwindSafe(self.handle_callback(&q))
            .catch_unwind()
            .await;
        self.bot.answer_callback_query(q.id.clone()).await?;
        if matches!(res, Err(_) | Ok(Err(_))) {
            self.send_alert().await?;
            self.send_message_public_chat("😬 Что-то пошло не так, но админ уже об этом знает")
                .await?;
            if let Ok(e) = res {
                return e;
            }
        }
        Ok(())
    }

    async fn handle_callback(&self, q: &CallbackQuery) -> Result<()> {
        let Some(data) = q.data.as_deref() else {
            return Ok(());
        };

        let author = Uid(q.from.id);

        if data.starts_with("/planvisit") {
            let visit_update = visits::parse_visit_text(author, util::strip_command(data))
                .expect("parsable date format in callback");
            self.backend()
                .plan_visit(visit_update.person, visit_update.day, visit_update.purpose)
                .await?;
        } else if data.starts_with("/unplanvisit") {
            let visit_update = visits::parse_visit_text(author, util::strip_command(data))
                .expect("parsable date format in callback");
            self.backend()
                .unplan_visit(visit_update.person, visit_update.day)
                .await?;
        } else if data == "/checkin" {
            self.backend().check_in(author, None).await?;
        } else if data == "/checkout" {
            self.backend().check_out(author).await?;
        } else {
            anyhow::bail!("unhandled callback query: {:?}", q);
        }

        Ok(())
    }
}
