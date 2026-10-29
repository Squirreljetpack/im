-- User accounts. Each user's data lives in their own Durable Object; D1 only
-- holds the credentials the Worker authenticates against.
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
