ALTER TABLE incoming_mentions
ADD first_mentioned TIMESTAMPTZ NOT NULL CONSTRAINT add_first_mentioned DEFAULT NOW();