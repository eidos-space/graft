"""Create disposable SQLite data and Graft history for table browser checks."""
import pathlib
import sqlite3
import subprocess
import sys


def note_text(index):
    if index % 5 == 0:
        return (
            "中文内容，支持自动换行。\nSecond line with a very long word: "
            + "abcdefghij" * 30
            + "\n"
            + "More detailed notes. " * 80
        )
    return "A short note." if index % 3 else "两行中文\n第二行"


if len(sys.argv) != 3:
    raise SystemExit("Usage: create-table-fixture.py NEW_DIRECTORY GRAFT_EXECUTABLE")
root = pathlib.Path(sys.argv[1]).resolve()
executable = str(pathlib.Path(sys.argv[2]).resolve())
if root.exists():
    raise SystemExit("Fixture path already exists; choose a fresh disposable directory")
root.mkdir(parents=True)
with sqlite3.connect(root / "data.sqlite") as connection:
    connection.execute(
        "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, "
        "body TEXT, optional TEXT, payload BLOB)"
    )
    connection.executemany(
        "INSERT INTO notes VALUES(?,?,?,?,?)",
        [(index, f"Note {index}", note_text(index), None, bytes([1, 2, 3]) * 30)
         for index in range(1, 3001)],
    )
    columns = ",".join(f"column_{index:02d} TEXT" for index in range(48))
    connection.execute(f"CREATE TABLE wide({columns})")
    placeholders = ",".join("?" for _ in range(48))
    connection.executemany(
        f"INSERT INTO wide VALUES({placeholders})",
        [[f"Row {row} / column {column}" for column in range(48)]
         for row in range(150)],
    )
    connection.execute("CREATE TABLE large(id INTEGER PRIMARY KEY, label TEXT)")
    connection.executemany("INSERT INTO large VALUES(?, ?)",
                           [(index, f"Large row {index}") for index in range(1, 100001)])
for arguments in [["init"], ["add", "-A"], ["commit", "-m", "Table browser test fixture"]]:
    subprocess.run([executable, *arguments], cwd=root, check=True, capture_output=True)
with sqlite3.connect(root / "data.sqlite") as connection:
    connection.execute(
        "UPDATE notes SET body='Changed 中文内容\n' || body WHERE id % 3 = 0"
    )
    connection.execute("DELETE FROM notes WHERE id BETWEEN 1 AND 10")
    connection.execute("UPDATE notes SET title='', optional='' WHERE id=11")
    connection.execute(
        "INSERT INTO notes VALUES(4000,'New note','Inserted value',NULL,X'ff00')"
    )
print(root)
