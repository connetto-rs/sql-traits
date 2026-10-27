CREATE TABLE orders (id INT PRIMARY KEY, total INT);
CREATE TABLE audit (id INT);
CREATE VIEW recent AS SELECT id, total FROM orders;
CREATE RULE log_order AS ON INSERT TO orders WHERE new.total > 0
    DO ALSO INSERT INTO audit VALUES (new.id);
CREATE RULE recent_insert AS ON INSERT TO recent
    DO INSTEAD INSERT INTO orders VALUES (new.id, new.total);
ALTER TABLE orders RENAME COLUMN total TO amount;
ALTER TABLE recent RENAME TO latest;
DROP VIEW latest;
