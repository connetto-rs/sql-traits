CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT, INSERT, UPDATE, DELETE ON docs TO app_reader;
CREATE POLICY shared ON docs USING (owner = ANY (string_to_array(current_setting('app.subjects', true), ';')));
