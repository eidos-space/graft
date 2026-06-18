-- exercise repository metadata pragmas through the SQLite extension
.connection 0
.open "file:app.db?vfs=graft"
.output /dev/null
pragma graft_init;
pragma graft_status;
pragma graft_json_status;
pragma graft_branch;
pragma graft_tags;
pragma graft_remotes;

.read datasets/bank.sql
pragma graft_status;
pragma graft_add;
pragma graft_commit = 'import bank dataset';
pragma graft_log;
pragma graft_json_log;
pragma graft_show = 'HEAD';
pragma graft_json_show = 'HEAD';

INSERT INTO ledger (account_id, amount) VALUES (1, -10), (2, 10);
pragma graft_status;
pragma graft_diff = 'HEAD';
pragma graft_json_diff = 'HEAD';
pragma graft_add;
pragma graft_commit = 'transfer between accounts';
pragma graft_log;

.output stdout
.echo on

SELECT COUNT(*) AS account_count FROM accounts;
SELECT COUNT(*) AS ledger_count FROM ledger;

.echo off
.output /dev/null
pragma graft_branch_create = 'reports';
pragma graft_switch_branch = 'reports';
pragma graft_status;
pragma graft_switch_branch = 'main';
pragma graft_branch;
