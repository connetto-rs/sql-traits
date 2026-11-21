CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE udocs (id INTEGER PRIMARY KEY, owner UUID NOT NULL);
ALTER TABLE udocs ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON udocs TO app_reader;
CREATE POLICY u ON udocs USING (owner = current_setting('app.user_id', true)::uuid);
