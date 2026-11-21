CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_writer NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON docs TO app_reader;
GRANT SELECT, INSERT ON docs TO app_writer;
CREATE POLICY sel ON docs FOR SELECT USING (true);
CREATE POLICY ins ON docs FOR INSERT TO app_reader WITH CHECK (owner = current_setting('app.user_id', true));
