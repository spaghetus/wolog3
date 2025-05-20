-- Add migration script here
CREATE TABLE guests (
    provider TEXT NOT NULL,
    sub TEXT NOT NULL,
    email TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY(provider, sub)
);
CREATE TABLE guestbook (
    provider TEXT NOT NULL,
    guest TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    post TEXT NOT NULL,
    PRIMARY KEY (provider, guest, post),
    FOREIGN KEY (provider, guest) REFERENCES guests(provider, sub) ON DELETE CASCADE,
    FOREIGN KEY(post) REFERENCES posts(path) ON DELETE CASCADE
);