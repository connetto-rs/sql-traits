CREATE ROLE app_reader NOLOGIN;
CREATE TABLE lines (order_id INTEGER NOT NULL, line_no INTEGER NOT NULL, owner TEXT NOT NULL, quantity INTEGER NOT NULL, PRIMARY KEY (order_id, line_no));
ALTER TABLE lines ENABLE ROW LEVEL SECURITY;
GRANT SELECT, INSERT, UPDATE, DELETE ON lines TO app_reader;
CREATE POLICY own ON lines USING (owner = current_setting('app.user_id', true)) WITH CHECK (owner = current_setting('app.user_id', true));
