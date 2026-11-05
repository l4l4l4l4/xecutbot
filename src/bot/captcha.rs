use super::*;
use chrono::Utc;
use sqlx::Row;
use teloxide::{
    ApiError, RequestError,
    types::{ChatPermissions, InlineKeyboardButton, InlineKeyboardMarkup, MessageEntity, UserId},
    payloads::SendMessageSetters,
};

impl<B: Backend> TelegramBot<B> {
    async fn delete_captcha_message(&self, message_id: Option<i32>) -> Result<()> {
        if let Some(id) = message_id {
            match self.bot.delete_message(self.config.public_chat_id, MessageId(id)).await {
                Ok(_) | Err(RequestError::Api(ApiError::MessageToDeleteNotFound)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) async fn start_captcha(&self, update: &ChatMemberUpdated) -> Result<()> {
        let _guard = self.captcha_lock.lock().await;
        let member = &update.new_chat_member;
        if member.is_owner() || member.is_administrator() {
            return Ok(());
        }
        let backend = self.backend();
        let pool = backend.pool();
        let joined_at = update.date.timestamp();
        let started_at: i64 = sqlx::query_scalar("SELECT started_at FROM antispam_start")
            .fetch_one(pool).await?;
        if joined_at <= started_at {
            return Ok(());
        }
        let user_id = member.user.id;
        if let Some(row) = sqlx::query("SELECT joined_at, message_id FROM captcha_pending WHERE user_id = ?")
            .bind(user_id.0 as i64).fetch_optional(pool).await?
        {
            if joined_at <= row.get::<i64, _>("joined_at") {
                return Ok(());
            }
            // Supersede the old intent before any Telegram call can fail.
            sqlx::query("UPDATE captcha_pending SET action = 'pending', joined_at = ?, expires_at = ? WHERE user_id = ?")
                .bind(joined_at).bind(Utc::now().timestamp() + 120).bind(user_id.0 as i64)
                .execute(pool).await?;
            self.delete_captcha_message(row.get("message_id")).await?;
        }
        sqlx::query("INSERT INTO captcha_pending (user_id, expires_at, joined_at) VALUES (?, ?, ?) ON CONFLICT(user_id) DO UPDATE SET message_id = NULL, expires_at = excluded.expires_at, joined_at = excluded.joined_at, action = 'pending'")
            .bind(user_id.0 as i64).bind(Utc::now().timestamp() + 120).bind(joined_at)
            .execute(pool).await?;
        self.bot.restrict_chat_member(self.config.public_chat_id, user_id, ChatPermissions::empty()).await?;
        let name = member.user.full_name();
        let text = format!("{name}, нажмите „Я человек“ в течение 2 минут, чтобы писать в чате.");
        let mention = MessageEntity::text_mention(member.user.clone(), 0, name.encode_utf16().count());
        let message = self.bot.send_message(self.config.public_chat_id, text)
            .entities(vec![mention])
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("Я человек", "/captcha")]]))
            .await?;
        if let Err(error) = sqlx::query("UPDATE captcha_pending SET message_id = ? WHERE user_id = ?")
            .bind(message.id.0).bind(user_id.0 as i64).execute(pool).await
        {
            if let Err(delete_error) = self.delete_captcha_message(Some(message.id.0)).await {
                log::error!("Failed to delete unsaved captcha: {delete_error:#}");
            }
            return Err(error.into());
        }
        Ok(())
    }

    pub(super) async fn handle_captcha_callback(&self, q: &CallbackQuery) -> Result<()> {
        let _guard = self.captcha_lock.lock().await;
        let Some(message) = q.regular_message() else {
            return Ok(());
        };
        if message.chat.id != self.config.public_chat_id {
            return Ok(());
        }
        let backend = self.backend();
        let row = sqlx::query("SELECT message_id, expires_at, action FROM captcha_pending WHERE user_id = ?")
            .bind(q.from.id.0 as i64).fetch_optional(backend.pool()).await?;
        let Some(row) = row else {
            return Ok(());
        };
        if row.get::<Option<i32>, _>("message_id") != Some(message.id.0)
            || row.get::<i64, _>("expires_at") <= Utc::now().timestamp()
            || row.get::<String, _>("action") != "pending"
        {
            return Ok(());
        }
        sqlx::query("UPDATE captcha_pending SET action = 'pass' WHERE user_id = ?")
            .bind(q.from.id.0 as i64).execute(backend.pool()).await?;
        self.finish_captcha(q.from.id, Some(message.id.0), "pass").await
    }

    async fn finish_captcha(&self, user_id: UserId, message_id: Option<i32>, action: &str) -> Result<()> {
        let member = self.bot.get_chat_member(self.config.public_chat_id, user_id).await?;
        if member.is_present() && !member.is_owner() && !member.is_administrator() {
            if action == "pass" {
                self.bot.restrict_chat_member(self.config.public_chat_id, user_id, ChatPermissions::all()).await?;
            } else {
                // Without only_if_banned Telegram removes members but permits rejoining.
                self.bot.unban_chat_member(self.config.public_chat_id, user_id).await?;
            }
        }
        self.delete_captcha_message(message_id).await?;
        sqlx::query("DELETE FROM captcha_pending WHERE user_id = ?")
            .bind(user_id.0 as i64).execute(self.backend().pool()).await?;
        Ok(())
    }

    async fn sweep_captchas(&self, ct: &CancellationToken) -> Result<()> {
        let _guard = tokio::select! {
            biased;
            _ = ct.cancelled() => return Ok(()),
            guard = self.captcha_lock.lock() => guard,
        };
        let backend = self.backend();
        let rows = sqlx::query("SELECT user_id, message_id, action FROM captcha_pending WHERE action != 'pending' OR expires_at <= ?")
            .bind(Utc::now().timestamp()).fetch_all(backend.pool()).await?;
        for row in rows {
            if ct.is_cancelled() {
                break;
            }
            let user_id = row.get::<i64, _>("user_id");
            let mut action: String = row.get("action");
            if action == "pending" {
                sqlx::query("UPDATE captcha_pending SET action = 'kick' WHERE user_id = ?")
                    .bind(user_id).execute(backend.pool()).await?;
                action = "kick".into();
            }
            if ct.is_cancelled() {
                break;
            }
            let result = tokio::select! {
                biased;
                _ = ct.cancelled() => return Ok(()),
                result = self.finish_captcha(UserId(user_id as u64), row.get("message_id"), &action) => result,
            };
            if let Err(error) = result {
                log::error!("Failed to finish captcha for {user_id}: {error:#}");
            }
        }
        Ok(())
    }

    pub(super) async fn captcha_task(self: Arc<Self>, ct: CancellationToken) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                biased;
                _ = ct.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    if let Err(error) = self.sweep_captchas(&ct).await {
                        log::error!("Failed to sweep captchas: {error:#}");
                    }
                }
            }
        }
    }
}
