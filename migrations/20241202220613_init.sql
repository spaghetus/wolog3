-- Add migration script here
CREATE TABLE posts (
    path TEXT PRIMARY KEY,
    updated TIMESTAMPTZ NOT NULL,
    ast JSONB NOT NULL,
    meta JSONB NOT NULL
);

CREATE TABLE errors (
    related_to_path TEXT PRIMARY KEY,
    FOREIGN KEY(related_to_path) REFERENCES posts(path)
);

CREATE TABLE incoming_mentions (
    to_path TEXT NOT NULL,
    from_url TEXT NOT NULL,
    last_mentioned TIMESTAMPTZ NOT NULL,
    PRIMARY KEY(to_path, from_url),
    FOREIGN KEY(to_path) REFERENCES posts(path) ON DELETE CASCADE
);