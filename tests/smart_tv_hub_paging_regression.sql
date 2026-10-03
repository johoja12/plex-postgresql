-- Run with psql -v ON_ERROR_STOP=1 against any isolated PostgreSQL database.
-- No persistent data is changed. Probes duplicates, mixed/all NULL dates,
-- descending order and page boundaries against Plex's first-occurrence logic.
BEGIN;
CREATE TEMP TABLE hub_episode_fixture(show_id integer, title_sort text, added_at integer);
INSERT INTO hub_episode_fixture VALUES
 (1,'Alpha',10),(1,'Alpha',20),(1,'Alpha',20),
 (2,'Bravo',NULL),(2,'Bravo',25),
 (3,'Charlie',NULL),(3,'Charlie',NULL),(4,'Zulu',20);
DO $$
DECLARE expected integer[]; actual integer[]; paged integer[];
BEGIN
 SELECT array_agg(show_id ORDER BY first_position) INTO expected FROM
  (SELECT show_id,min(ord) first_position FROM
   unnest(ARRAY(SELECT show_id FROM hub_episode_fixture ORDER BY added_at DESC,title_sort))
   WITH ORDINALITY AS ids(show_id,ord) GROUP BY show_id) d;
 SELECT ARRAY(SELECT show_id FROM hub_episode_fixture GROUP BY show_id,title_sort
  ORDER BY CASE WHEN bool_or(added_at IS NULL) THEN NULL ELSE max(added_at) END DESC,title_sort)
 INTO actual;
 SELECT ARRAY(SELECT show_id FROM hub_episode_fixture GROUP BY show_id,title_sort
  ORDER BY CASE WHEN bool_or(added_at IS NULL) THEN NULL ELSE max(added_at) END DESC,title_sort LIMIT 2 OFFSET 0)
 || ARRAY(SELECT show_id FROM hub_episode_fixture GROUP BY show_id,title_sort
  ORDER BY CASE WHEN bool_or(added_at IS NULL) THEN NULL ELSE max(added_at) END DESC,title_sort LIMIT 2 OFFSET 2)
 INTO paged;
 IF expected <> ARRAY[2,3,1,4] OR actual <> expected OR paged <> expected THEN
  RAISE EXCEPTION 'Hub ordering/paging mismatch: expected %, actual %, paged %',expected,actual,paged;
 END IF;
END $$;
ROLLBACK;
