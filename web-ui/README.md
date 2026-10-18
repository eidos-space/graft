# Graft local WebUI

React + TypeScript UI embedded in the native `graft` executable. It uses
`@pierre/trees` for the explorer and `@pierre/diffs` for code, unified diffs,
and text editing, and Glide Data Grid for SQLite rows. SQLite inspection uses Graft's resident SDK and read-only
physical database connections.

```sh
pnpm --dir web-ui install --frozen-lockfile
pnpm --dir web-ui build
cargo build -p graft-cli --locked
target/debug/graft ui /path/to/repository --no-open
```

Rebuild the UI **before** the executable after changing frontend sources.
`generated/` is ignored by Git. CI and CLI release jobs build it before compiling
Rust; local source builds must run the commands above first. Prebuilt CLI binaries
include the UI and need no Node installation. The build generates an embedded asset registry for the entry point
and all Vite chunks. Text diffs, the editor, syntax grammars and
themes load on demand. Hash-named chunks are cached; the HTML entry point,
`app.js` and repository API responses are never cached.
Opening an existing folder as `graft .` is shorthand for `graft ui .`.

Development builds optimize the Fjall and LSM dependencies while retaining
debuggable application code. Without these overrides, recovering a substantial
local store can take several seconds even before the viewer prints its URL.
To measure startup against an idle repository or an isolated repository copy:

```sh
node web-ui/scripts/benchmark-startup.mjs --repo /path/to/repository --budget-ms 1500
```

The benchmark performs one warmup and three measured starts, reports time to
loading output and URL readiness, and stops only the processes it started.
It does not open a browser or print session credentials. The optional budget is
machine-specific; release builds retain their existing optimization settings.

The host binds to `127.0.0.1`, chooses an available port, and creates an
ephemeral bearer token carried in the printed URL fragment. The UI removes
the fragment after loading and keeps the token in tab session storage. The
host checks Host, Origin, and token for API calls and excludes repository
metadata, traversal, and symbolic links outside the worktree.
When restarting on a fixed port, opening the new full URL in the same tab also
replaces the old session credentials without requiring a page reload.

The explorer and open worktree file refresh every three seconds. History loads
when its tab is first opened; commit versions are immutable. Tree models persist across mode changes and apply
path additions/removals without resetting folder expansion. Editing pauses
content refresh and blocks file switching. Saves check the original content
hash, prepare a replacement in the same folder, recheck the hash, and replace
the file without staging or committing. SQLite browsing is read only.
While any save, branch mutation, or merge completion is pending, navigation is
blocked and snapshot/merge polling pauses. Pending text saves also disable the
editor and Cancel/Escape, so an in-flight write cannot discard later keystrokes
or finish against an unmounted file. Failed reads expose a retry action. A failed
detail renderer leaves the repository browser available for opening another file.

Text diffs provide Wrap and Unified / Split controls in the file header.
These preferences apply to Changes and History and are saved in browser storage
for the current local origin.

Drag the divider to resize the repository browser, or focus it and use the arrow
keys. The footer's layout button moves the browser between the left and right.
Width and position are saved in the browser across local ports; narrow screens stack the panes.

The branch selector in the footer lists local branches and supports creating
and switching branches through the repository command service. Switching uses
normal worktree protections and is blocked while a text edit is open.
An active merge adds a Conflicts tab with paged file and conflict lists. Text
conflicts use Pierre's `UnresolvedFile` and its built-in per-block current,
incoming and both choices. A three-way preview retains nonconflicting edits.
Save and mark resolved is enabled after every block has been handled; saving
checks the file version and SDK merge state token before writing and staging
the result. SQLite conflicts reuse the virtualized Glide table, preserving the
original table columns and one database record per row. Conflicting cells show
Current and Incoming choices inline; row headers offer whole-row selection.
Click Current/Incoming inside the cell to choose a side. Click the value text
or its expand arrow to read both complete values in the Pierre diff dialog,
with wrapping, all unchanged context expanded and the same choice controls. Opening and closing the
dialog preserves the selection; Escape returns focus to the grid. Review drafts
are scoped to the merge and file, retained across navigation and tab refresh,
and cleared after successful saving. Saving blocks navigation and pauses merge
polling until the operation finishes. Enter opens
a focused conflict cell; Space switches its side. Row headers use Enter/Space
to switch whole-row choices. Nonconflicting fields retain their merged values. Row choices
use the SDK's whole-row resolution.
The repository's `merge.same_row_merge` policy must be enabled before starting
a merge to produce cell conflicts for competing updates. Insert/delete and
ordinary row conflicts require whole-row choices. Selections remain local until
every loaded conflict is handled, then saving applies them with a refreshed state token
after each successful operation. A failed save retains the remaining choices;
completed operations are durable. Conflict pages load on demand rather than
reading the entire database up front; loaded selections can be saved before
requesting more conflicts. A database is marked resolved only when the SDK has
no unresolved conflicts left. Resolved database files retain a Glide table
viewer for the saved result. Structural conflicts require CLI resolution.
Binary and text deletion conflicts retain whole-file side selection with
an explicit in-app confirmation. The viewer shows the resulting text after
resolution. Complete merge creates the
merge commit only after all conflicts are resolved. Mutations validate the
SDK merge state token; concurrent changes require a fresh review.

SQLite files appear as expandable containers in the same Pierre explorer.
Their table children are virtual UI nodes: selecting one opens the real database
with that table selected, and the breadcrumb includes its literal SQL name.
All files lists every table; Changes and History list tables with row, schema,
or opaque changes for that comparison, including empty added/deleted tables.
Database nodes start collapsed and do not expand when a file is selected or when
history opens. Table lists load only after manual expansion, without returning row
data. Rows load only after a table is selected. Filtering does not expand or read
unopened databases. Database catalog, row and diff requests share one read queue;
obsolete queued requests are cancelled before opening a database.

The sidebar footer groups Compact folders, Expand folders and Collapse folders
in one shared toolbar for Files, Changes and the selected History commit's tree.
Compact folders combines single-child directories as `a/b/c`; it preserves file
selection and directory expansion and is saved with the browser's display
preferences. Expand folders opens ordinary directories without reading unopened
databases. Collapse folders also closes database containers.
The selected database reuses metadata already included in its table response.
Live expanded lists refresh with the worktree; historic lists are scoped to
their commit. SQL names containing path separators remain single tree leaves
and are never passed to the host as filesystem paths.

Press Shift + Cmd + L to switch the entire UI between dark and light themes.
The selected theme is saved in the browser across local ports. Display preferences
use a host-scoped cookie, with localStorage as a fallback; session tokens remain
separate in per-tab sessionStorage. The keyboard button in the footer opens a
shortcut reference. Escape or clicking outside closes it and restores focus.
Press Shift + Cmd + F (Ctrl on other platforms) to focus the current browser
filter. All files and Changes filter their directory trees; History searches
commit messages and hashes across repository history with paginated matches.
Filters stay separate for each tab. Escape clears and closes the current filter.

Inspection is bounded: text up to 2 MB, file inventory up to 20,000 paths,
history pages of 40 commits, commit path pages of 100, and database pages of
100 rows per request. Tables expose their full row count, with continuous virtual
scrolling and automatic page reads. Database text is limited to 4096 characters by
the host; the UI labels shortened previews. Double-click or press Enter on a
regular cell to read the complete value using the source viewer. Detail reads
check the database version before and after reading to avoid mismatched rows.
Values exceeding the 2 MB detail limit show an explicit error instead of a
truncated value. Unchanged database refreshes use the main-file/WAL version to
skip row scans, and background tabs pause periodic reads. Row diffs use cursor
pagination and show loaded progress; they do not imply random access to rows
that have not yet been fetched.
Viewport page reads carry the initial database version. If an external writer
changes the database, the host rejects the page instead of combining rows from
different versions; live refresh replaces the cache with the new version.
The row-diff view also exposes schema and unsupported-surface information
from the SDK. All text, including HTML and SVG, uses the source diff/editor;
HTML is never rendered as a page. Worktree raster images, audio, video, and PDF
have dedicated previews. Audio and video use native browser controls and
authenticated byte-range reads; codec support follows the browser. PDF.js is
loaded only when a PDF is opened. Historical binary content has no preview;
absent local payloads are reported without implicitly fetching remote content.

Before the first commit, SQLite diff uses the index as its baseline and falls
back to staged changes when the worktree matches the index. The UI labels
this case; after a first commit it compares HEAD with the worktree.

Validation:

```sh
pnpm --dir web-ui check
pnpm --dir web-ui test
cargo test -p graft-cli webui:: --locked
cargo clippy -p graft-cli --all-targets --locked --no-deps
```

The reusable `DataTable` and `TableDiff` components use Glide's canvas grid with
the same semantic colors and monospace type as file diffs. Columns sample the
first page and adapt within 100–360px; users can also drag column boundaries.
Both rows and columns are virtualized. Ordinary tables support arbitrary scroll
positions and retain at most 32 fetched pages plus the initial page. Switching
tables or refreshing changed data cancels obsolete requests and invalidates pages.
Glide loads only when a database is opened.

`TableDiff` accepts SDK row changes; it does not read or write repositories.
Current-data blobs appear as byte counts. SDK diff values encode blobs as hex
strings without cell type metadata, so the diff retains those strings rather
than guessing which text is binary. Diff expansion reveals the values supplied
by the SDK; current-data expansion reads complete text through the version-checked
detail endpoint.
Diffs follow the SDK's cursor automatically until the requested range is available.
Unlike ordinary table offset reads, a distant diff position needs preceding cursor
pages. Diff pages remain available for switching filters and returning to earlier rows.

To exercise real-data table regression checks, create a disposable fixture:

```sh
python3 web-ui/tests/create-table-fixture.py /tmp/graft-table-test target/debug/graft
# After building UI and CLI, start the host and use its printed URL:
target/debug/graft ui /tmp/graft-table-test --no-open
UI_URL='http://127.0.0.1:PORT/#token=TOKEN' pnpm --dir web-ui test:table:browser
```

The test needs an installed Playwright Chromium, or `CHROMIUM_PATH` pointing to
an existing Chromium executable. `SCREENSHOT_DIR` chooses the screenshot folder
(default `/private/tmp/graft-glide-screenshots`). It checks jumping directly to
row 100,000, bounded visible rows, 48-column scrolling, table switches, automatic
diff cursor pagination and resizing against the real local API.

Row-diff tabs appear in the file header, in the same position as text diff
display controls. They combine All / Updates / Added / Deleted with All fields /
Changed fields. Counts refer to the complete SDK summary, and selecting a filter
automatically loads enough cursor pages for the visible matches. Changed fields
keeps all columns until all pages are known, then keeps rowid/key and the union
of columns changed in matching rows. Inserts and deletions treat every field as
changed because the complete row was added or removed. Tab groups support arrow
keys, Home and End, with one keyboard tab stop per group.

Updated cells show before/after previews and a small comparison control; clicking
it or double-clicking the cell opens a read-only detail dialog with the
same Pierre word/line diff renderer used for files. The dialog starts with Split
and Wrap, supports Unified, and shows the complete values provided by the SDK.
Escape or the close button returns keyboard focus to the table without moving
its scroll position. These display choices are local to the detail dialog.
