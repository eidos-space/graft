.echo on
.open "file:app.db?vfs=graft"
.output /dev/null
pragma graft_init;
.output stdout
-- Legacy pre-repository Volume pragmas are not part of the public SQLite API.
pragma graft_volume_push;
