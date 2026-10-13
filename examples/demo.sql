CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT);
INSERT INTO users (id, name, age) VALUES (1, 'ada', 36), (2, 'grace', 45);
SELECT name, age FROM users WHERE age > 40 ORDER BY age DESC;
UPDATE users SET age = 37 WHERE id = 1;
DELETE FROM users WHERE id = 2;
SELECT COUNT(*), AVG(age) FROM users;