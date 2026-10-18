CREATE TABLE decentrala_delivery (
    event_date TEXT NOT NULL,
    event_time TEXT NOT NULL,
    title TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    thread_id INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('sending', 'sent', 'uncertain', 'failed')),
    caption TEXT NOT NULL,
    message_id INTEGER,
    error TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (event_date, event_time, title, chat_id, thread_id),
    CHECK (status != 'sent' OR message_id IS NOT NULL)
);
