CREATE TABLE issue854 AS SELECT ARRAY[NULL, 'a']::text[] AS data;
CREATE INDEX idxissue854 ON issue854 USING zombodb ((issue854.*));
DROP TABLE issue854;