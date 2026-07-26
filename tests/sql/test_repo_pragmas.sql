.echo on
.open "file:project/app.db?vfs=graft"
CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
-- Repository control belongs to the graft CLI, not the SQLite VFS.
pragma graft_status;
