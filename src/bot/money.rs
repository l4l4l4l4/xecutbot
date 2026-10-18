use super::Command;
use crate::{
    backend::Backend,
    money::{Claim, Money, Month, euros, parse_euros, private_gate, reaction_approved},
};
use anyhow::{Result, bail, ensure};
use std::{sync::Arc, time::Duration};
use teloxide::{
    payloads::{SendPhotoSetters as _, SetMessageReactionSetters as _},
    prelude::Requester as _,
    requests::HasPayload as _,
    sugar::request::RequestReplyExt as _,
    types::{
        ChatId, ChatMemberUpdated, InputFile, Message, MessageId, MessageReactionUpdated,
        ReactionType, UserId,
    },
    utils::command::BotCommands,
};
use tokio_util::sync::CancellationToken;

pub(super) fn parse_command(text: &str, username: &str) -> Option<Command> {
    let (command, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let name = command.split('@').next().unwrap_or(command);
    // Normalize only the new commands' separator, then delegate suffix/arguments to teloxide.
    if matches!(name, "/money" | "/help") {
        Command::parse(&format!("{command} {}", args.trim()), username).ok()
    } else {
        Command::parse(text, username).ok()
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn count(arg: Option<&str>, default: usize, max: usize) -> Result<usize> {
    let n = arg.map(str::parse).transpose()?.unwrap_or(default);
    ensure!((1..=max).contains(&n), "number must be 1..{max}");
    Ok(n)
}
fn financial_help(username: &str) -> String {
    format!(
        r#"<b>EUR ledger — private resident chat only</b>
/money@{username} — balance, chart and financier contact
/money@{username} balance [months] — 1..36 months; configured default 12
/money@{username} pay AMOUNT [YYYY-MM ...] — positive EUR, at most 2 decimals; current month if omitted; 1..36 unique months. Split equally, with remaining cents assigned to the first months.
/money@{username} paid [months] — emoji table; default 6, max 12
/money@{username} history [limit] — financier only; default 20, max 100
/money@{username} income AMOUNT DESCRIPTION — financier only
/money@{username} expense AMOUNT DESCRIPTION — financier only
/money@{username} reverse TRANSACTION_ID REASON — financier only; audited counterentry, no deletion
/money@{username} members add USER_ID [YYYY-MM] — financier only; verifies membership; optional imported join month, otherwise current month
/money@{username} card CLAIM_ID — financier only; reply to the original bot payment card to recover its binding after interrupted delivery
/money@{username} retrycard CLAIM_ID — financier only; resend a card ONLY after checking no original exists. Telegram sends are not idempotent.

A pay command creates an immutable <b>pending</b> claim, not cash. After receiving funds, the financier adds 👍 to the <b>bot card</b>. Only the configured numeric financier ID can approve. The bot adds 👌 after the durable ledger commit; failed acknowledgements are retried automatically and on restart. Removing a reaction never reverses cash: use reverse. Edited commands never change claims.
✅ fully paid; 🟡 partial; 🕓 pending only; ❌ due; ➖ no obligation. Pending amounts do not count as paid. New members have no earlier debt; departed members retain history. One EUR wallet, opening zero.
The chart shows monthly closing balances in Europe/Belgrade, using confirmation/posting dates, not allocated contribution months. Opening carry-in is included.
A full initial roster import is required: Telegram cannot enumerate every resident. An incomplete-roster warning means silent members may be missing. Missing minimum, numeric financier, accounting start or verified roster prevents finance startup. No username-based authority."#
    )
}

impl<B: Backend> super::TelegramBot<B> {
    pub(super) async fn initialize_money(&self) -> Result<()> {
        if !self.config.money.enabled {
            return Ok(());
        }
        let money = Money::new(self.backend().pool().clone(), self.config.money.clone())?;
        let me = self.bot.get_me().await?;
        let membership = self
            .bot
            .get_chat_member(self.config.private_chat_id, me.id)
            .await?;
        ensure!(
            membership.is_privileged(),
            "finance requires bot administrator access in private_chat_id for chat_member and message_reaction updates"
        );
        let initialized: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM money_settings WHERE id=1)")
                .fetch_one(&money.pool)
                .await?;
        if !initialized {
            // Verify the explicit import before initializing a zero-opening wallet.
            for id in &self.config.money.initial_member_ids {
                let member = self
                    .bot
                    .get_chat_member(self.config.private_chat_id, UserId(*id))
                    .await?;
                ensure!(
                    member.is_present() && !member.user.is_bot,
                    "initial roster ID {id} must be a current human private-chat member"
                );
            }
        }
        money.initialize().await?;
        let ids = sqlx::query_scalar::<_, i64>("SELECT user_id FROM money_members")
            .fetch_all(&money.pool)
            .await?;
        for id in ids {
            let member = self
                .bot
                .get_chat_member(self.config.private_chat_id, UserId(id as u64))
                .await?;
            // Reconcile known accounts on restart; active imports keep their original join dates.
            money
                .observe(
                    id,
                    &member.user.full_name(),
                    member.user.is_bot,
                    member.is_present(),
                    crate::datetime::now().date_naive(),
                )
                .await?;
        }
        let _ = self.money.set(money);
        if let Err(e) = self.retry_money_receipts().await {
            log::warn!("Money receipt pending after startup: {e:#}");
        }
        Ok(())
    }
    pub(super) async fn money_receipt_task(self: Arc<Self>, ct: CancellationToken) -> Result<()> {
        if !self.config.money.enabled {
            ct.cancelled().await;
            return Ok(());
        }
        loop {
            tokio::select! { _=ct.cancelled()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(30))=>{if let Err(e)=self.retry_money_receipts().await {log::warn!("Money receipt remains pending: {e:#}");}} }
        }
    }
    async fn retry_money_receipts(&self) -> Result<()> {
        let Some(money) = self.money.get() else {
            return Ok(());
        };
        for (id, chat, card) in money.receipts().await? {
            if chat != self.config.private_chat_id.0 {
                continue;
            }
            let sent = self
                .bot
                .set_message_reaction(ChatId(chat), MessageId(card))
                .reaction(vec![ReactionType::Emoji {
                    emoji: "👌".into()
                }])
                .await;
            match sent {
                Ok(_) => money.acknowledged(id).await?,
                Err(e) => log::warn!("Claim {id} committed; acknowledgement pending: {e}"),
            }
        }
        Ok(())
    }
    pub(super) async fn observe_money_message(self: Arc<Self>, msg: Message) {
        if let Err(e) = self.observe_financial_member(&msg).await {
            log::error!("Cannot observe financial member: {e:#}");
        }
    }
    async fn observe_financial_member(&self, msg: &Message) -> Result<()> {
        if msg.chat.id != self.config.private_chat_id || msg.sender_chat.is_some() {
            return Ok(());
        }
        let Some(user) = &msg.from else { return Ok(()) };
        let Some(money) = self.money.get() else {
            return Ok(());
        };
        let date = msg
            .date
            .with_timezone(&chrono_tz::Europe::Belgrade)
            .date_naive();
        money
            .observe(user.id.0 as i64, &user.full_name(), user.is_bot, true, date)
            .await?;
        if let Some(users) = msg.new_chat_members() {
            for user in users {
                money
                    .observe(user.id.0 as i64, &user.full_name(), user.is_bot, true, date)
                    .await?;
            }
        }
        if let Some(user) = msg.left_chat_member() {
            money
                .observe(
                    user.id.0 as i64,
                    &user.full_name(),
                    user.is_bot,
                    false,
                    date,
                )
                .await?;
        }
        Ok(())
    }
    pub(super) async fn handle_money_member(
        self: Arc<Self>,
        update: ChatMemberUpdated,
    ) -> Result<()> {
        if update.chat.id != self.config.private_chat_id {
            return Ok(());
        }
        if let Some(money) = self.money.get() {
            let member = &update.new_chat_member;
            money
                .observe(
                    member.user.id.0 as i64,
                    &member.user.full_name(),
                    member.user.is_bot,
                    member.is_present(),
                    update
                        .date
                        .with_timezone(&chrono_tz::Europe::Belgrade)
                        .date_naive(),
                )
                .await?;
        }
        Ok(())
    }
    pub(super) async fn handle_money_reaction(
        self: Arc<Self>,
        update: MessageReactionUpdated,
    ) -> Result<()> {
        let Some(money) = self.money.get() else {
            return Ok(());
        };
        let approval = |reactions: &[ReactionType]| {
            reactions
                .iter()
                .any(|reaction| matches!(reaction,ReactionType::Emoji{emoji} if emoji=="👍"))
                .then_some("👍")
        };
        let old = approval(&update.old_reaction);
        let new = approval(&update.new_reaction);
        if reaction_approved(
            update.chat.id.0,
            self.config.private_chat_id.0,
            update.user().filter(|u| !u.is_bot).map(|u| u.id.0),
            money.financier as u64,
            old.as_slice(),
            new.as_slice(),
        ) {
            money
                .confirm(update.chat.id.0, update.message_id.0, money.financier)
                .await?;
            self.retry_money_receipts().await?;
        }
        Ok(())
    }
    pub(super) async fn handle_help(&self, msg: &Message, args: &str) -> Result<()> {
        if args.trim().is_empty() {
            let me = self.bot.get_me().await?;
            self.send_message_reply(
                msg,
                escape(&Command::descriptions().username_from_me(&me).to_string()),
            )
            .await?;
        } else if args.trim() == "money" {
            if private_gate(msg.chat.id.0, self.config.private_chat_id.0) {
                let me = self.bot.get_me().await?;
                self.send_message_reply(msg, financial_help(me.username()))
                    .await?;
            } else {
                self.send_message_reply(
                    msg,
                    "Financial help is available only in the private resident chat.",
                )
                .await?;
            }
        } else {
            self.send_message_reply(msg, "Use /help@xecutbot or /help@xecutbot money.")
                .await?;
        }
        Ok(())
    }
    pub(super) async fn handle_money(&self, msg: &Message, args: &str) -> Result<()> {
        if !private_gate(msg.chat.id.0, self.config.private_chat_id.0) {
            self.send_message_reply(
                msg,
                "Financial commands are available only in the private resident chat.",
            )
            .await?;
            return Ok(());
        }
        let Some(money) = self.money.get() else {
            self.send_message_reply(msg,"Finance is disabled. Configure money.enabled, minimum_monthly_euros, numeric financier_user_id, start_month and the full initial_member_ids roster; see README.").await?;
            return Ok(());
        };
        let Some(user) = msg.from.as_ref().filter(|u| !u.is_bot) else {
            return Ok(());
        };
        if msg.sender_chat.is_some() {
            self.send_message_reply(
                msg,
                "Use your own non-anonymous account for financial commands.",
            )
            .await?;
            return Ok(());
        }
        self.observe_financial_member(msg).await?;
        let result = self.money_command(money, msg, user.id.0 as i64, args).await;
        if let Err(e) = result {
            self.send_message_reply(
                msg,
                format!(
                    "❌ {}\nUse /help@xecutbot money for arguments and approval flow.",
                    escape(&e.to_string())
                ),
            )
            .await?;
        }
        Ok(())
    }
    async fn roster_warning(&self, money: &Money) -> Result<String> {
        let count = self
            .bot
            .get_chat_member_count(self.config.private_chat_id)
            .await? as i64;
        let known = money.known_active_count().await?;
        // getChatMemberCount includes this bot even when it has never sent a message.
        let me = self.bot.get_me().await?;
        let observed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM money_members WHERE user_id=?)")
                .bind(me.id.0 as i64)
                .fetch_one(&money.pool)
                .await?;
        let known = known + i64::from(!observed);
        Ok(if known == count {
            String::new()
        } else {
            format!(
                "\n⚠️ Incomplete roster: {known} known active accounts (including known bots), Telegram reports {count}. Silent or unknown accounts may be missing; financier must reconcile/import the full roster.\n"
            )
        })
    }
    async fn money_command(
        &self,
        money: &Money,
        msg: &Message,
        actor: i64,
        args: &str,
    ) -> Result<()> {
        let mut words = args.split_whitespace();
        let action = words.next().unwrap_or("balance");
        let financier = || -> Result<()> {
            ensure!(
                actor == money.financier,
                "only the configured numeric financier may use this command"
            );
            Ok(())
        };
        match action {
            "balance" => {
                let months = count(words.next(), money.config.chart_months, 36)?;
                ensure!(words.next().is_none(), "usage: balance [months]");
                let warning = self.roster_warning(money).await?;
                self.send_message_reply(msg,format!("💶 Wallet: <b>{}</b>\nOpening: 0.00 EUR; pending claims excluded.\nFinancier: {}{}",euros(money.balance().await?),escape(&money.config.contact),warning)).await?;
                let points = money.graph(Month::current(), months).await?;
                self.bot.send_photo(msg.chat.id,InputFile::memory(crate::money::chart_png(&points)?).file_name("eur-wallet.png"))
                    .caption("EUR wallet — monthly closing balances in Europe/Belgrade, with opening carry-in")
                    .with_payload_mut(|p|p.message_thread_id=msg.thread_id).reply_to(msg.id).await?;
            }
            "pay" => {
                let amount = parse_euros(
                    words
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("usage: pay AMOUNT [YYYY-MM ...]"))?,
                )?;
                let mut months = words.map(Month::parse).collect::<Result<Vec<_>>>()?;
                if months.is_empty() {
                    months.push(Month::current())
                }
                let _card_guard = self.money_card_lock.lock().await;
                let claim = money
                    .claim(msg.chat.id.0, msg.id.0, actor, amount, &months)
                    .await?;
                if money.reserve_card(claim.id).await? {
                    self.send_claim_card(money, msg, &claim).await?;
                } else if claim.card_id.is_some() {
                    self.send_message_reply(
                        msg,
                        format!(
                            "Claim #{} already recorded; the original card is unchanged.",
                            claim.id
                        ),
                    )
                    .await?;
                } else {
                    self.send_message_reply(msg,format!("Claim #{} is persisted, but card delivery is uncertain. Ask the financier to reply /money@xecutbot card {} to the original card, or explicitly use /money@xecutbot retrycard {} only after checking no card was sent.",claim.id,claim.id,claim.id)).await?;
                }
            }
            "paid" => {
                let months = count(words.next(), 6, 12)?;
                ensure!(words.next().is_none(), "usage: paid [months]");
                let last = Month::current();
                let first = last.shift(1 - months as i32);
                let mut header = format!("{:<40}", "Person [ID]");
                for n in 0..months {
                    header.push_str(&format!(" {} ", first.shift(n as i32)));
                }
                let mut table = header.clone();
                for (id, name) in money.members().await? {
                    let label = format!("{} [{}]", name.chars().take(18).collect::<String>(), id);
                    let mut row = format!("\n{label:<40}");
                    for n in 0..months {
                        row.push_str(&format!(
                            "   {}    ",
                            money.status(id, first.shift(n as i32)).await?.emoji()
                        ));
                    }
                    if table.len() + row.len() > 2800 {
                        self.send_message_reply(msg, format!("<pre>{}</pre>", escape(&table)))
                            .await?;
                        table = header.clone();
                    }
                    table.push_str(&row);
                }
                self.send_message_reply(msg,format!("<pre>{}</pre>\n✅ fully paid · 🟡 partial · 🕓 pending · ❌ due · ➖ not required{}",escape(&table),self.roster_warning(money).await?)).await?;
            }
            "history" => {
                financier()?;
                let limit = count(words.next(), 20, 100)?;
                ensure!(words.next().is_none(), "usage: history [limit]");
                let entries = money.history(limit).await?;
                if entries.is_empty() {
                    self.send_message_reply(msg, "No posted transactions; opening 0.00 EUR.")
                        .await?;
                }
                for entry in entries {
                    self.send_message_reply(
                        msg,
                        format!(
                            "#{} · {} · {}\n{} · actor {}{}\n{}",
                            entry.id,
                            euros(entry.amount),
                            entry.kind,
                            escape(&entry.posted),
                            entry.actor,
                            entry
                                .reverse_id
                                .map(|id| format!(" · reverses #{id}"))
                                .unwrap_or_default(),
                            escape(&entry.description)
                        ),
                    )
                    .await?;
                }
            }
            "income" | "expense" => {
                financier()?;
                let amount = parse_euros(
                    words
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("usage: {action} AMOUNT DESCRIPTION"))?,
                )?;
                let description = itertools::Itertools::join(&mut words, " ");
                let id = money
                    .entry(
                        msg.chat.id.0,
                        msg.id.0,
                        actor,
                        if action == "expense" { -amount } else { amount },
                        action,
                        &description,
                    )
                    .await?;
                self.send_message_reply(
                    msg,
                    format!(
                        "Posted transaction #{id}. Wallet: {}",
                        euros(money.balance().await?)
                    ),
                )
                .await?;
            }
            "reverse" => {
                financier()?;
                let id = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("usage: reverse TRANSACTION_ID REASON"))?
                    .parse()?;
                let reason = itertools::Itertools::join(&mut words, " ");
                let counter = money
                    .reverse(msg.chat.id.0, msg.id.0, actor, id, &reason)
                    .await?;
                self.send_message_reply(
                    msg,
                    format!(
                        "Counterentry #{counter} reverses #{id}. Wallet: {}",
                        euros(money.balance().await?)
                    ),
                )
                .await?;
            }
            "members" => {
                financier()?;
                ensure!(
                    words.next() == Some("add"),
                    "usage: members add USER_ID [YYYY-MM]"
                );
                let id: u64 = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing member ID"))?
                    .parse()?;
                ensure!(id > 0 && id <= i64::MAX as u64, "invalid numeric member ID");
                let joined = words
                    .next()
                    .map(Month::parse)
                    .transpose()?
                    .unwrap_or_else(Month::current);
                ensure!(
                    joined >= money.start && joined <= Month::current(),
                    "join month must be between accounting start and current month"
                );
                ensure!(
                    words.next().is_none(),
                    "usage: members add USER_ID [YYYY-MM]"
                );
                let member = self
                    .bot
                    .get_chat_member(self.config.private_chat_id, UserId(id))
                    .await?;
                ensure!(
                    member.is_present() && !member.user.is_bot,
                    "user must be a current human private-chat member"
                );
                money
                    .observe(id as i64, &member.user.full_name(), false, true, joined.0)
                    .await?;
                self.send_message_reply(
                    msg,
                    format!(
                        "Member {id} imported. Existing membership history preserved.{}",
                        self.roster_warning(money).await?
                    ),
                )
                .await?;
            }
            "card" | "retrycard" => {
                financier()?;
                let id: i64 = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing claim ID"))?
                    .parse()?;
                ensure!(words.next().is_none(), "usage: {action} CLAIM_ID");
                let _card_guard = self.money_card_lock.lock().await;
                let claim = money.find_claim(id, msg.chat.id.0).await?;
                ensure!(
                    claim.card_id.is_none() && !claim.confirmed,
                    "claim already has its immutable card or is confirmed"
                );
                if action == "card" {
                    let card = msg
                        .reply_to_message()
                        .ok_or_else(|| anyhow::anyhow!("reply to the original bot payment card"))?;
                    let me = self.bot.get_me().await?;
                    let expected = format!(
                        "{}\n{}",
                        self.card_text(money, &claim).await?,
                        money.config.contact
                    );
                    ensure!(
                        card.from.as_ref().is_some_and(|u| u.id == me.id)
                            && card.text() == Some(expected.as_str()),
                        "reply must be the exact original immutable bot card for this claim"
                    );
                    money.attach_card(id, card.id.0).await?;
                } else {
                    let reserved=sqlx::query("INSERT OR IGNORE INTO money_card_attempts(chat_id,source_id,claim_id) VALUES(?,?,?)").bind(msg.chat.id.0).bind(msg.id.0).bind(id).execute(&money.pool).await?.rows_affected();
                    ensure!(
                        reserved == 1,
                        "this recovery attempt was already sent; do not resend an ambiguous delivery"
                    );
                    self.send_claim_card(money, msg, &claim).await?;
                }
                self.send_message_reply(
                    msg,
                    format!("Claim #{id} card recovered; financier must add 👍 to that card."),
                )
                .await?;
            }
            _ => bail!("unknown /money subcommand"),
        }
        Ok(())
    }
    async fn card_text(&self, money: &Money, claim: &Claim) -> Result<String> {
        let allocations = money
            .allocations(claim.id)
            .await?
            .iter()
            .map(|(month, amount)| format!("{month}: {}", euros(*amount)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "💶 Payment claim #{}\nResident ID {} · {}\n{}\nPending: no cash posted yet.\nFinancier ID {}: add 👍 to this bot card only after receiving funds. The bot adds 👌 after durable confirmation.",
            claim.id,
            claim.user_id,
            euros(claim.amount),
            allocations,
            money.financier
        ))
    }
    async fn send_claim_card(&self, money: &Money, msg: &Message, claim: &Claim) -> Result<()> {
        // Telegram has no sendMessage idempotency key: reserve BEFORE I/O, never automatically resend ambiguous delivery.
        let text = self.card_text(money, claim).await?;
        let card = self
            .send_message_reply(
                msg,
                format!(
                    "{}\n<a href=\"tg://user?id={}\">{}</a>",
                    escape(&text),
                    money.financier,
                    escape(&money.config.contact)
                ),
            )
            .await?;
        money.attach_card(claim.id, card.id.0).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suffix_and_whitespace_use_native_bot_commands() {
        for text in [
            "/money@xecutbot pay 50 2026-10",
            "/money@xecutbot\tpay 50 2026-10",
            "/money@xecutbot\npay 50 2026-10",
        ] {
            assert!(
                matches!(parse_command(text,"xecutbot"),Some(Command::Money(args)) if args=="pay 50 2026-10")
            );
        }
        assert!(parse_command("/money@otherbot pay 50", "xecutbot").is_none());
        assert!(
            matches!(parse_command("/help@xecutbot\nmoney","xecutbot"),Some(Command::Help(args)) if args=="money")
        );
    }
}
