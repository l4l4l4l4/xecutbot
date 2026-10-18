# xecut_bot

A Telegram bot for managing hackerspace visits and other things.

## Running

Create a config file `xecut_bot.yaml`. See `xecut_bot_example.yaml`.

Make sure you have Rust and Cargo installed (for example with rustup).

Then run:

```sh
cargo run --release
```

or build with `cargo build --release` and use the executable directly:

```sh
target/release/xecut_bot
```

## EUR money ledger

All money commands and financial replies are restricted to `telegram_bot.private_chat_id`
(the private resident group, not arbitrary direct messages). `/help@xecutbot` is available anywhere;
`/help@xecutbot money` is restricted to that group. Native teloxide parsing supports bot suffixes,
for example `/money@xecutbot pay 50 2026-10` and `/help@xecutbot money`.
Existing visit/status commands remain available; the update command stays hidden.

Finance is disabled by default. Before enabling `telegram_bot.money.enabled`, supply:

- `minimum_monthly_euros`: the **actual agreed** positive EUR amount as a quoted decimal
  with at most two decimal places. No minimum is assumed.
- `financier_user_id`: the financier's verified positive **numeric Telegram user ID**.
  The contact defaults to `@rvbar_el3`; a username never grants authority.
- `start_month`: the agreed first accounting month (`YYYY-MM`), opening wallet **0.00 EUR**.
- `initial_member_ids`: **every human member** of the private resident group, including
  silent members. Set `initial_roster_complete: true` only after checking this full import.
  Telegram's Bot API cannot enumerate a complete member list. IDs are checked with
  `getChatMember` before first initialization; bots and absent members are rejected.

Missing setup fails closed with an actionable error before the bot/backend is constructed.
The bot must be an **administrator** in the private resident group. The dispatcher requests
`message`, `chat_member`, `message_reaction` and existing callback updates through its
teloxide update-filter hints (`allowed_updates`); reactions and membership changes need
admin access. Observe all human group messages and joins/leaves; do not rely on command
users alone. Bot accounts owe nothing. Names/IDs in the imported roster are shown even
if those residents have never sent a message.

The displayed roster warning compares known active accounts (including known bots) with
`getChatMemberCount`. A count mismatch means the roster is incomplete or needs reconciliation;
even a matching count is not proof of identity completeness. The explicit operator import
is the source of initial completeness. New human members start owing in their joining month,
not before; departures preserve historical obligations through the departure month.
`/money@xecutbot members add USER_ID [YYYY-MM]` lets the numeric financier import a missing current
human member after Telegram membership verification. Optional month supplies the known
join month; existing membership intervals are preserved.

Known accounts, including bots, are reconciled again at startup. Imported active residents keep their
original joining month. A departure missed while offline is recorded when observed; Telegram
cannot reconstruct the missing historical departure date.

### Commands and receipt flow

- `/money@xecutbot` or `/money@xecutbot balance [months]`: EUR balance, financier contact and native PNG chart
  (`chart_months`, default 12; accepted range 1–36).
- `/money@xecutbot pay AMOUNT [YYYY-MM ...]`: immutable pending payment card. Without months, use
  the current month. Allocate exact cents equally across 1–36 unique months, with remainder
  cents going to the first specified months. Months must be between accounting start and
  36 months ahead. Reject duplicates, invalid decimals, zero/negative amounts and overflow.
- `/money@xecutbot paid [months]`: every known human's person/month table (default 6, max 12).
  ✅ fully paid, 🟡 partial, 🕓 only awaiting approval, ❌ due/unpaid, ➖ not required.
- `/money@xecutbot history [limit]`: financier only, posted audit entries (default 20, max 100).
- `/money@xecutbot income AMOUNT DESCRIPTION`, `/money@xecutbot expense AMOUNT DESCRIPTION`: financier only.
- `/money@xecutbot reverse TRANSACTION_ID REASON`: financier only, audited counterentry. Original
  records remain; reversing a payment removes its month credit. Never reverse twice or
  reverse a reversal. Removing a Telegram approval reaction does not undo a transaction.
- `/help@xecutbot money`: full arguments, error limits, roles and reaction instructions.

After transferring actual funds, the financier adds **👍 to the bot payment card**, not the
resident's command. Only the configured numeric user and newly added approval in the private
group are accepted. Pending claims never change cash or paid totals. Confirmation, ledger
entry and claim status commit atomically in SQLite; concurrent approvals, reaction replay,
duplicate commands and restart cannot repost the same money. Only after commit does the bot
add **👌**. Failed receipts remain durably pending and retry on startup and every 30 seconds.
Do not edit the immutable card/source to correct cash: use an audited reversal.

Telegram has no `sendMessage` idempotency key. The bot persists a card-send reservation before
network I/O, so restart/replay never automatically duplicates a potentially delivered card.
If sending was interrupted, the financier can reply `/money@xecutbot card CLAIM_ID` to the exact original
bot card to recover its binding. Only after explicitly checking that no original exists may
the financier use `/money@xecutbot retrycard CLAIM_ID`; duplicate recovery-command updates never resend.
This is an operator-authorized recovery from ambiguous delivery, not an automatic retry.

### Accounting policy and storage

Amounts are integer cents; ledger entries record posting timestamps independently of contribution
target months. Accounting months and observed membership dates use `Europe/Belgrade`; stored
posting timestamps remain UTC. The native PNG shows monthly closing balances with embedded
bitmap text (no browser, host fonts or external service), includes opening carry-in, and never
invents history before `start_month`. The default window is one year, shortened when accounting
began more recently.
Wallet/start and past minimum policies are persistent. Do not change `start_month`.
To change the minimum, set the actual new `minimum_monthly_euros` and a new
`minimum_effective_month` (current or later). Existing month policies cannot be overwritten;
old debts are never retroactively recalculated.

Migration `20261013020000_money.sql` adds constrained `money_settings`, `money_policies`,
`money_members`, `money_memberships`, `money_claims`, `money_allocations`, `money_ledger`
and `money_card_attempts` tables to the existing SQLite database. Back up that database
before deploying migrations. Keep the database intact across restarts; dropping it loses
ledger history and idempotency receipts. No production activation is implied by enabling
the example: actual policy, numeric financier and complete roster are operator prerequisites.

### Development checks

```sh
make test  # SQLX_OFFLINE=true cargo test --locked
make check # ledger tests, dispatcher smoke, formatting check and strict clippy
```

### Repeatable dispatcher smoke

```sh
make smoke      # matching Rust/OpenSSL toolchain and Python 3 available
make smoke-nix  # installs the development tools from this repository's flake.lock pin
# Without make/Rust on the host PATH:
bash tests/smoke_nix.sh
```

`tests/money_smoke.py` launches the real Rust dispatcher with a dummy token, a
free loopback port and a fresh temporary SQLite database. The Rust launcher
refuses non-loopback transports and any database other than that generated
temporary ledger. It never calls Telegram or uses production configuration.
First-time tool/dependency downloads may require network access; bot traffic is
strictly local. Temporary processes, listener and database are cleaned on exit.

The scenario checks pending versus paid money, silent imported residents,
numeric-ID authority despite a matching username, repeated/conflicting claims,
foreign-chat/anonymous/unauthorized expenses, durable confirmation before 👌,
failed acknowledgement and restart recovery, income, expenses, audited
reversals, partial-payment status, displayed balance and the actual annual PNG.
The fixture minimum of EUR 50 and IDs 101/202/303 are synthetic, not deployment values.

After all assertions pass, inspect `target/smoke-money/transcript.html` in a browser,
`target/smoke-money/balance.png` and `target/smoke-money/report.json`.
These are actual bot responses and image bytes from that run, not a Telegram
client. Existing artifacts from an earlier successful run do not validate a
later failed run. `make check` runs both the ledger suite and this dispatcher smoke.

### Manual Telegram smoke

Use a **separate test bot/token and private test group**, with a resident account
and a different financier account. Do not launch another polling process with
the production token. Make the test bot an administrator and allow 👍 and 👌.

Keep credentials in ignored `target/manual-money/xecut_bot.yaml` (mode 0600).
Start from the example configuration; replace **every** Telegram destination
with a test destination, set `db.sqlite_path` to
`sqlite:target/manual-money/ledger.sqlite?mode=rwc`, `rest_api.bind_address` to
`127.0.0.1:0`, and keep `backend.enable_update: false`. Supply both real test
account IDs, a test contact, quoted test minimum and current accounting month;
only then enable money. Do not inherit production `XECUT_BOT*` environment overrides.
Launch with `cargo run --locked -- --config target/manual-money/xecut_bot.yaml`.

Use the **test bot's actual username** in command suffixes:
1. `/help@TESTBOT money`, `/money@TESTBOT` — zero wallet and 12-month chart.
2. Resident: `/money@TESTBOT pay 50` — pending card, wallet still zero.
3. Resident 👍 on the card — no payment. Financier 👍 — EUR 50, then bot 👌.
4. `/money@TESTBOT paid 3` — payer ✅, silent unpaid financier ❌.
5. Remove/re-add 👍 and restart with the same test database — no duplicate money.
6. Financier: `expense 10 test`, then `history` and `reverse ID test` through
   `/money@TESTBOT` — EUR 40, then EUR 50, with both audit entries retained.
7. Try an expense as resident and `/money` in another test chat — no ledger change.

All payments in this room are simulated. Keep this database separate from the
real wallet and never copy its records into production. This manual Telegram
flow is not exercised by the credential-free loopback target.

## License
MIT
