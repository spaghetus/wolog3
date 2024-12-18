-- Add migration script here
CREATE VIEW visible_posts AS
SELECT *
FROM posts
WHERE meta->>'hidden' = 'false';