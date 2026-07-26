.echo on
.open "file:app.db?vfs=graft"
-- Legacy pre-repository Volume pragmas are not part of the public SQLite API.
pragma graft_volume_push;
