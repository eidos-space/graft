.echo off
.output /dev/null
.open "file:project/app.db?vfs=graft"
pragma graft_init;
pragma graft_status;
CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
INSERT INTO accounts (name) VALUES ('Alice');
pragma graft_status;
pragma graft_add;
pragma graft_status;
pragma graft_commit = 'initial accounts';
pragma graft_status;
pragma graft_diff;
INSERT INTO accounts (name) VALUES ('Bob');
pragma graft_add;
pragma graft_commit = 'add bob';
pragma graft_branch_create = 'feature/search';
pragma graft_switch_branch = 'feature/search';
INSERT INTO accounts (name) VALUES ('Carol');
pragma graft_add;
pragma graft_commit = 'feature row';
pragma graft_switch_branch = 'main';
.output stdout
.echo on

SELECT COUNT(*) AS main_count FROM accounts;

.echo off
.output /dev/null
pragma graft_branch;
pragma graft_diff = 'main feature/search -- app.db';
pragma graft_log;
pragma graft_show = 'HEAD';
pragma graft_switch_branch = 'feature/search';
.output stdout
.echo on

SELECT COUNT(*) AS feature_count FROM accounts;
SELECT name FROM accounts ORDER BY id;
