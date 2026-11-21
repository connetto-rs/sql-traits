CREATE ROLE app_owner_editor NOLOGIN;
CREATE ROLE app_body_writer NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, body TEXT);
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
GRANT SELECT (id, owner), UPDATE (owner) ON docs TO app_owner_editor;
GRANT SELECT (id, owner), UPDATE (body) ON docs TO app_body_writer;
CREATE POLICY open ON docs USING (true);
