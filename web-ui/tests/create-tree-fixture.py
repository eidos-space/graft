"""Create a disposable repository for virtual database-tree UI verification."""
import pathlib
import sqlite3
import subprocess
import sys

if len(sys.argv) != 3:
    raise SystemExit("Usage: create-tree-fixture.py NEW_DIRECTORY GRAFT_EXECUTABLE")
root = pathlib.Path(sys.argv[1]).resolve()
executable = str(pathlib.Path(sys.argv[2]).resolve())
if root.exists():
    raise SystemExit("Fixture path already exists; choose a new disposable directory")
root.mkdir(parents=True)
(root / "README.md").write_text("# Table review\nA disposable database-tree example.\n")


def graft(*arguments):
    subprocess.run([executable, *arguments], cwd=root, check=True, capture_output=True)


with sqlite3.connect(root / "data.sqlite") as connection:
    connection.executescript('''
        CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, body TEXT);
        INSERT INTO notes VALUES(1, 'Review launch', 'Before the agent edit');
        CREATE TABLE unchanged(id INTEGER PRIMARY KEY, body TEXT);
        INSERT INTO unchanged VALUES(1, 'Unchanged table');
        CREATE TABLE empty_removed(id);
        CREATE TABLE "a/b % 中文"(id INTEGER PRIMARY KEY, body TEXT);
        INSERT INTO "a/b % 中文" VALUES(1, 'Before');
    ''')
graft("init")
graft("add", "-A")
graft("commit", "-m", "Initial database")
with sqlite3.connect(root / "data.sqlite") as connection:
    connection.executescript('''
        UPDATE notes SET body='Recorded agent change' WHERE id=1;
        CREATE TABLE empty_added(id);
        DROP TABLE empty_removed;
    ''')
graft("add", "-A")
graft("commit", "-m", "Review table changes")
with sqlite3.connect(root / "data.sqlite") as connection:
    connection.executescript('''
        UPDATE notes SET body='New working change' WHERE id=1;
        UPDATE "a/b % 中文" SET body='Changed 中文内容' WHERE id=1;
        CREATE TABLE working_only(id);
    ''')
print(root)
