.echo off
.output /dev/null
.open "file:app.db?vfs=graft"

CREATE TABLE t1(a, b);
INSERT INTO t1 VALUES(1, 2);
INSERT INTO t1 VALUES(3, 4);
INSERT INTO t1 VALUES(5, 6);

.output stdout
.echo on

SELECT COUNT(*) AS row_count FROM t1;
SELECT * FROM t1 ORDER BY a;

.echo off
.output /dev/null
vacuum;
drop table t1;
vacuum;
