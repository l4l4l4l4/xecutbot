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

## Decentrala weekly announcements

This optional feature is disabled unless `telegram_bot.decentrala.enabled` is
`true`. Older configurations without the section still work and do not start an
announcement network task. See `xecut_bot_example.yaml` for all overrides.

Default schedule: Thursday at **12:00 Europe/Belgrade**, to residents chat
`-1002614784999`, forum topic `14828`. Each event on the **next Monday** at Xecut
gets its own logo photo and HTML caption. Other venues/days are excluded; all
matching events are included. Serbian `tema`, source date/time and optional
source link are used directly, without translation or generated descriptions.
Quoted CSV commas and surrounding field whitespace are supported. A malformed
date anywhere in the source rejects the check with its row number. A valid
header with no matches sends nothing; an empty/corrupt response is an error.

The scheduler checks every minute. It catches up a missed weekly slot on
restart, but only until the target Monday begins locally. It uses the configured
IANA timezone, so Belgrade's DST keeps 12:00 local time. For nondefault slots,
a nonexistent DST wall time is skipped; a repeated wall time uses its first
occurrence. Shutdown cancels sleeps, fetches and in-flight delivery. A delivery
interrupted after reservation is blocked rather than resent automatically.

`templates/decentrala.j2` is the separate editable MiniJinja 2 template.
Default template and PNG are bundled in the executable, so a packaged service
needs no repository working directory. Set `template` and/or `image` to absolute
runtime paths to override them; templates are read at each scheduled check.
`source` accepts an HTTP(S) CSV URL or a local file. Relative paths resolve from
the process working directory. HTML autoescaping is forced for external values,
source links must be HTTP(S), and overlong captions fail before any send.
Keep custom templates within Telegram's supported HTML and caption limit.

### No-send preview

These commands do not load bot credentials/configuration, open SQLite or call
Telegram. `--date` is the **announcement date**, so `2026-10-15` selects
Monday `2026-10-19`. Each rendered caption goes to stdout; no matches is reported
on stderr. The scheduler uses exactly the same selection/rendering functions.

```sh
# Offline, two Xecut events; no network access:
cargo run -- decentrala-preview --date 2026-10-15 --source tests/fixtures/decentrala.csv
# Live authoritative CSV (fetch only; never sends):
cargo run -- decentrala-preview --date 2026-10-15
# Runtime template override:
cargo run -- decentrala-preview --date 2026-10-15 --source tests/fixtures/decentrala.csv --template templates/decentrala.j2
```

After building, `target/release/xecut_bot decentrala-preview ...` works the same
way, including outside the repository when using bundled defaults.

### Durable delivery and manual reconciliation

The existing SQLite database stores `decentrala_delivery`, keyed by event date,
source time/title, configured chat and topic. A unique atomic reservation is
committed **before** Telegram I/O; confirmed `sent` rows retain the message ID.
Restarts and concurrent checks cannot resend the same event. Identical source
rows are collapsed. Source link/template changes do not change event identity;
changing a title/time or destination creates a distinct identity, so operators
must reconcile edits to already-announced events before restarting.

States: `sending` means an attempt was reserved (possibly interrupted);
`sent` means Telegram returned a message and the receipt was saved;
`failed` means Telegram explicitly rejected the request; `uncertain` means a
network/I/O/unreadable-response error may have occurred after acceptance.
**None of these rows are automatically resent.** Errors are logged and stored
on the row. Fetch/parser/template/image errors before reservation can be
retried safely. Failure to persist a successful receipt leaves `sending`, which
also blocks retries. Other independent events still get their own attempts.

Before repairing state, disable this task/stop the bot and back up SQLite.
Inspect the destination topic and the saved caption:

```sql
SELECT event_date, event_time, title, chat_id, thread_id, status, message_id,
       error, caption FROM decentrala_delivery ORDER BY event_date, event_time;
```

If a blocked event was actually delivered, record its actual Telegram message
ID and set `status='sent', error=NULL` on that **exact primary-key row**.
If it is definitely absent (including an explicit rejection), delete only its
exact primary-key row to permit a fresh attempt after fixing the cause and
re-enabling. Never clear all state or infer absence from a timeout. Re-enabling
only catches up while that week's target Monday is still in the future;
later delivery is intentionally not automatic.

### Logo attribution

`assets/decentrala-logo.svg` is Decentrala's authentic
[logo-light.svg](https://gitea.dmz.rs/Decentrala/website/src/branch/master/site/img/logo-light.svg),
not the forge-generated organization avatar. `assets/decentrala-logo.png` is a
640×640 white-background rasterization of that SVG; no generated imagery.
The [upstream website licence](https://gitea.dmz.rs/Decentrala/website/src/branch/master/LICENCE)
is **GNU AGPL version 3**, included as `assets/decentrala-LICENCE`.
The bot's existing licence below is unchanged; the bundled upstream artwork is
attributed and licensed separately.

## Verification

Run `make check` for the tests, formatting check and Clippy; `make test` runs
the tests alone. The commands use the committed dependency lock and offline
SQLx metadata, without bot credentials. Install a matching Rust toolchain with
rustfmt and Clippy, plus pkg-config and OpenSSL development libraries.

### Repeatable announcement smoke

```sh
make smoke      # matching Rust/OpenSSL toolchain available
make smoke-nix  # development tools from this repository's flake.lock pin
# Without make/Rust on the host PATH:
bash tests/smoke_nix.sh
```

Runs the delivery/selection regressions against temporary SQLite and loopback
Telegram, then renders both fixture events through the editable Jinja2 template.
The fixture's announcement date is Thursday 2026-10-15 and target Monday is
2026-10-19. Bot credentials and production configuration are never loaded;
the fixture preview performs no network fetch or Telegram sends. First-time
tool/dependency downloads may require network access.

For a real Telegram presentation check, use a **separate test bot/token and
private test forum**. Replace every Telegram destination in the example,
including `decentrala.chat_id` and `decentrala.thread_id`, with actual test
destinations; do not use residents topic 14828. Use a fresh ignored database
under `target/manual-decentrala/`, `rest_api.bind_address: "127.0.0.1:0"` and
`backend.enable_update: false`; clear production `XECUT_BOT*` environment overrides.
Keep the test config/token in an ignored file with mode 0600. After inspecting
the no-send preview, enable announcements and set weekday/time to a due test
slot in Europe/Belgrade, with a source containing an event on the selected
future Monday. Launch with `cargo run --locked -- --config PATH_TO_TEST_CONFIG`.
Check image, source text, links and topic in Telegram, then restart the same
test database: the event must not send again. This real-client check is manual
and is not performed by `make smoke`.

## License
MIT
