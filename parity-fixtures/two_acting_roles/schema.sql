CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_editor NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON docs TO app_reader;
GRANT SELECT, UPDATE ON docs TO app_editor;
CREATE POLICY own ON docs FOR SELECT TO app_reader USING (owner = current_setting('app.user_id', true));
CREATE POLICY edit_read ON docs FOR SELECT TO app_editor USING (true);
CREATE POLICY edit ON docs FOR UPDATE TO app_editor USING (true);
