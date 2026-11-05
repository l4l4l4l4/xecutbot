CREATE TABLE IF NOT EXISTS chat_senders (
    user_id INTEGER NOT NULL PRIMARY KEY,
    first_message_pending INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE antispam_start (
    started_at INTEGER NOT NULL
);

INSERT INTO antispam_start (started_at) VALUES (unixepoch());
