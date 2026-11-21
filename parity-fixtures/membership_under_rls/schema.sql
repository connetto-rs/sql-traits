CREATE ROLE app_reader NOLOGIN;
CREATE ROLE app_admin NOLOGIN;
CREATE TABLE members (team TEXT NOT NULL, user_id TEXT NOT NULL, PRIMARY KEY (team, user_id));
CREATE TABLE tdocs (id INTEGER PRIMARY KEY, team TEXT NOT NULL);
ALTER TABLE members ENABLE ROW LEVEL SECURITY;
ALTER TABLE tdocs ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON members, tdocs TO app_reader;
CREATE POLICY m_self ON members USING (user_id = current_setting('app.user_id', true));
CREATE POLICY t_team ON tdocs USING (EXISTS (SELECT 1 FROM members m WHERE m.team = tdocs.team AND m.user_id = current_setting('app.user_id', true)));
