-- exercise the intentionally narrow VFS pragma surface
.connection 0
.open "file:app.db?vfs=graft"
.output /dev/null
pragma graft_version;
pragma graft_debug_volume_info;
pragma graft_debug_volume_json_info;

.read datasets/bank.sql
INSERT INTO ledger (account_id, amount) VALUES (1, -10), (2, 10);

.output stdout
.echo on

SELECT COUNT(*) AS account_count FROM accounts;
SELECT COUNT(*) AS ledger_count FROM ledger;

.echo off
