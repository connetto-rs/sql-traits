CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT, INSERT, UPDATE, DELETE ON docs TO app_reader;
GRANT app_admin TO app_reader;
CREATE POLICY own ON docs USING (owner = current_setting('app.user_id', true));
CREATE POLICY admin ON docs TO app_admin USING (true);
