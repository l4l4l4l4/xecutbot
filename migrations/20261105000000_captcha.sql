DROP TABLE chat_senders;

CREATE TABLE captcha_pending (
    user_id INTEGER PRIMARY KEY,
    message_id INTEGER,
    expires_at INTEGER NOT NULL,
    joined_at INTEGER NOT NULL,
    action TEXT NOT NULL DEFAULT 'pending' CHECK(action IN ('pending', 'pass', 'kick'))
);
