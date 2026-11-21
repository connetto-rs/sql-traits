CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE jdocs (id INTEGER PRIMARY KEY, data JSONB NOT NULL);
ALTER TABLE jdocs ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON jdocs TO app_reader;
CREATE POLICY j ON jdocs USING ((data ->> 'owner') = current_setting('app.user_id', true));
