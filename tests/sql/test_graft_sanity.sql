.echo off
.output /dev/null
.open "file:app.db?vfs=graft"
pragma graft_init;

CREATE TABLE t1(a, b);
INSERT INTO t1 VALUES(1, 2);
INSERT INTO t1 VALUES(3, 4);

pragma graft_status;
pragma graft_add;
pragma graft_commit = 'initial rows';
pragma graft_status;

INSERT INTO t1 VALUES(5, 6);
pragma graft_diff = 'HEAD';
pragma graft_add;
pragma graft_commit = 'add third row';
pragma graft_log;
pragma graft_show = 'HEAD';

.output stdout
.echo on

SELECT COUNT(*) AS row_count FROM t1;
SELECT * FROM t1 ORDER BY a;

.echo off
.output /dev/null
vacuum;
drop table t1;
vacuum;
pragma graft_status;
