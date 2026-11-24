-- Shared by ordinary claims and expiry payouts, including when payout mode is disabled.
CREATE TABLE watchman_sweep_index (
	id INTEGER PRIMARY KEY CHECK (id = 1),
	next_index BIGINT NOT NULL CHECK (next_index BETWEEN 0 AND 2147483648)
);
INSERT INTO watchman_sweep_index (id, next_index) VALUES (1, 0);
