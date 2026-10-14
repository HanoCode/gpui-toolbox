-- Body backfill walks a folder newest-first by local row id while skipping rows that
-- already have a cached body. This partial index keeps that walk cheap: once the
-- cursor moves past a row, the row drops out of the index entirely, so a batch never
-- re-scans the already-cached part of a large mailbox.
--
-- Companion to 011_pending_body_page.sql, which covers the "newest N" prefetch
-- (`ORDER BY date DESC, id DESC`). The backfill path orders by id alone, so it needs
-- its own index rather than degrading into a full folder scan.
CREATE INDEX idx_messages_body_backfill
  ON messages(folder_id, id DESC)
  WHERE uid IS NOT NULL AND body_state = 'none';
