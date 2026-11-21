CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT, INSERT, UPDATE, DELETE ON docs TO app_reader;
CREATE POLICY selfref ON docs USING (EXISTS (SELECT 1 FROM docs d2 WHERE d2.id = docs.id AND d2.owner = current_setting('app.user_id', true)));
