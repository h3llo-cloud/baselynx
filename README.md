# baselynx

A fast desktop client for PostgreSQL, for Windows, macOS and Linux, that also speaks MySQL, MariaDB and
SQLite. Rust, with the UI on GPUI through [GPUI Kit](https://gpui-kit.com). It starts instantly, connects to
plain servers and to databases inside Kubernetes (through its own port-forwards), and takes and restores
backups.

PostgreSQL is the first-class engine: every feature works there first and best. MySQL/MariaDB and SQLite are
supported: browsing, editing, the console, export, backups and restores all work; a few Postgres-specific
extras do not (see [Engines](#engines)).

Free software under the [Apache License 2.0](LICENSE).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/hero-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="docs/images/hero-light.png">
  <img alt="baselynx: a filtered customers table with the record panel open" src="docs/images/hero-dark.png">
</picture>

- Several databases connected at once, grouped in projects you switch between; Kubernetes databases grouped
  by cluster in a sidebar you can filter and drive from the keyboard
- Query console: run the statement at the caret, a selection, or the whole script; cancel; completion of
  tables and columns from the loaded catalog; errors underlined where the server points; one result tab per
  statement; a history of every statement run
- Guarded writes: on red and amber connections a write runs in a transaction baselynx opens, with Commit and
  Roll back, and an UPDATE or DELETE without WHERE asks first
- Table view: WHERE filter with column completion and history, server-side sort, pages, row count, refresh
  and auto-refresh that tints what changed since the last load
- Edit cells, add and delete rows, undo and redo, review the generated SQL, commit in one transaction
- Select many cells of one column (Shift+click, Ctrl+click, Shift+Up/Down) and give them one value, or paste
  a copied column into them
- Follow foreign keys (F4, Alt+click, Space to peek), walk back with Alt+[; a record panel (Ctrl+I) shows the
  selected row top to bottom
- The same table on another connection in one click; read-only mode through the replica
- Export to CSV or Excel, the current page or every row matching the filter; copy a row as INSERT, JSON or
  tab-separated text
- One config file, `baselynx.yaml`, for Kubernetes and plain databases, opened at start; scheduled backups
  (dump or SQL) and guarded restores
- PostgreSQL first; MySQL, MariaDB and SQLite files too, side by side in the same project
- Ctrl+K jumps to any table of any connected database, a row (`customers 42`), a recent query (`>`), or a
  command

## A tour

**Edit like a spreadsheet.** Select a range in a column with Shift+click, add cells with Ctrl+click, type
once: every selected cell takes the value, as one undo step. On a production (red) connection the commit
shows the exact SQL first.

![Selecting cells in a column, typing one value, reviewing the UPDATEs](docs/images/bulk-edit.gif)

**Get anywhere fast.** Menus open under the pointer. Ctrl+K finds any table of any connected database,
a recent query, a command, or a row: `customers 42` opens that customer.

![Menus opening on hover, then Ctrl+K jumping to customers 42](docs/images/navigation.gif)

**Query console.** Completion from the catalog, errors underlined where the server points, a result tab per
statement, and guarded writes on red and amber connections.

![The query console with a revenue-by-country query and its result](docs/images/console.png)

| | |
|---|---|
| ![Ctrl+K palette searching for "ord"](docs/images/palette.png) | ![Commit review listing the generated UPDATE statements](docs/images/commit-review.png) |
| **Ctrl+K** finds tables across every connection | **Commit review** shows the SQL before it runs |
| ![Backups view with schedules, last backups and states](docs/images/backups.png) | ![Restore dialog with every guardrail checked](docs/images/restore.png) |
| **Backups** on a schedule, with status per database | **Restores** only after every guardrail passes |

![The config file editor](docs/images/config-editor.png)

One `baselynx.yaml` describes every server and Kubernetes database, with schedules and modes; the app edits
and checks it in place.

## Install

Prebuilt for Windows (x64), macOS (Apple silicon and Intel) and Linux (x64 and arm64); no Rust needed.

```sh
# macOS and Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/h3llo-cloud/baselynx/releases/latest/download/baselynx-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/h3llo-cloud/baselynx/releases/latest/download/baselynx-installer.ps1 | iex"
```

The installers put `baselynx` in `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows), add it to `PATH`, and
install `baselynx-update` beside it: run `baselynx-update` to move to the latest release. On Windows there is
also an `.msi` on the [releases page](https://github.com/h3llo-cloud/baselynx/releases), with a Start menu entry;
to update, install the newer `.msi` over it.

The builds are not code-signed yet. Installed with the commands above they start normally; a downloaded `.msi`
shows a SmartScreen warning ("More info", then "Run anyway"). On Linux the app needs a Wayland or X11 session
with a Vulkan driver, and `libxkbcommon`, `libxkbcommon-x11`, `libwayland`, `fontconfig` and `freetype`, which
desktop installations have.

Installed, baselynx keeps its data in the configuration directory (see
[Where things are stored](#where-things-are-stored)); File → Open config file… adds a `baselynx.yaml` as a project.

## Quick start from source

```sh
git clone <this repository> baselynx && cd baselynx
cp baselynx.example.yaml baselynx.yaml     # then edit it: your servers or clusters
cargo run --release -p baselynx
```

A `baselynx.yaml` in the repository root opens at every start of a build from `target/`. Without one,
baselynx starts empty: add a connection, or paste a `postgres://` URL into the sidebar. Instructions for
coding agents are in [llms.txt](llms.txt).

## Build

Rust 1.92 or later (the toolchain used here is 1.99). The first build takes several minutes.

```sh
cargo build --release -p baselynx      # binary: target/release/baselynx(.exe)
cargo run -p baselynx                  # debug build
```

Per system:

- **Windows 10 or later:** Visual Studio 2022 Build Tools with the "Desktop development with C++" workload,
  the MSVC Rust toolchain, and CMake on `PATH`. This is the only system the build has been verified on.
- **macOS 15 or later:** Xcode Command Line Tools (`xcode-select --install`).
- **Linux:** a Wayland or X11 session with a working Vulkan driver, and on Ubuntu 24.04:

  ```sh
  sudo apt install -y gcc g++ clang libfontconfig-dev libwayland-dev libwebkit2gtk-4.1-dev \
    libxkbcommon-x11-dev libx11-xcb-dev libssl-dev libzstd-dev libasound2-dev \
    vulkan-validationlayers libvulkan1
  ```

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The SQLite tests need nothing. The MySQL/MariaDB tests in `crates/baselynx-core/tests/mysql.rs` run only with
`BASELYNX_TEST_MYSQL_DSN=mysql://root:password@127.0.0.1:3306/` (a user who may create databases); their backup
test also needs `mysqldump` and `mysql`, on PATH or in `BASELYNX_TEST_MYSQL_TOOLS`. Without the variable they
pass without connecting.

The other integration tests in `crates/baselynx-core/tests` need a PostgreSQL server:

- With `BASELYNX_TEST_DSN=postgres://user:password@host:5432/postgres` they use that server. The role must be
  allowed to create databases.
- Without it they download PostgreSQL once (network needed on the first run), install it under
  `target/baselynx-testdb`, and start it on `127.0.0.1:54329`. The server keeps running after the tests so later
  runs start quickly. Stop it with
  `target/baselynx-testdb/install/<version>/bin/pg_ctl -D target/baselynx-testdb/data stop`.

`fixtures/seed.sql` is loaded once into a template database `baselynx_seed`; every test run works on a fresh copy.

The window tests in `crates/baselynx-app/src/ui_tests.rs` use GPUI Kit's `test-support` feature and the same server.
That feature only resolves with two exact pins, kept in `Cargo.lock`: `taffy 0.15.0` (`gpui-fast` asks for `=0.15.0`
while `gpui-pre` would take 0.15.1) and `unicode-properties 0.1.3` (`gpui-fast-web` asks for `=0.1.3`). A
`cargo update` that moves either breaks `cargo test`; restore them with
`cargo update -p taffy --precise 0.15.0` and `cargo update -p unicode-properties --precise 0.1.3`.

## Engines

| | PostgreSQL | MySQL / MariaDB | SQLite |
|---|---|---|---|
| Connect | servers, Kubernetes | servers, Kubernetes | a file on disk |
| Browse, filter, sort, page, count | yes | yes | yes |
| Edit rows, commit in one transaction | yes | yes (InnoDB) | yes |
| Console, completion, history, cancel | yes | yes (`KILL QUERY`) | yes |
| Error underlined where the server points | at the character | — (MySQL gives a line, not a position) | at the character, when SQLite says |
| Guarded writes (commit or roll back) | yes | yes, but schema changes commit at once | yes |
| Read-only mode | yes, through the replica | yes, through the replica | the file opened read-only |
| Column statistics (the glance card) | yes | — | — |
| Pooler detection (PgBouncer) | yes | — | — |
| Backups | `dump` and `sql`, with pg_dump | `sql`, with mysqldump | a file copy and `sql`, no tools needed |
| Restores | one transaction | not all-or-nothing (schema changes commit at once) | one transaction |

**Servers that speak the PostgreSQL protocol** (TimescaleDB, Supabase, Neon, AlloyDB, Amazon RDS and Aurora
PostgreSQL) are PostgreSQL underneath and work as PostgreSQL. CockroachDB and YugabyteDB also speak it, but
their catalogs differ in places; browsing and the console should work, editing may not. We have not tested
these ourselves yet: reports welcome.

## The config file

baselynx reads its databases from `baselynx.yaml`. [baselynx.example.yaml](baselynx.example.yaml) describes every key;
in short:

```yaml
version: 1
project: Shop
backups:
  directory: backups
clusters:
  shop-prod:
    kubeconfig: ~/.kube/shop-prod.yaml
    namespaces:
      billing:                       # a Kubernetes namespace
        orders-db:                   # shown as "shop-prod · orders-db"
          service: orders-db-rw
          replica: orders-db-ro
          credentials: orders-db-credentials
          mode: read-write           # or read-only
          backup: {every: 24h, formats: [dump, sql]}
servers:
  analytics: postgres://report:secret@db.example.com/analytics?sslmode=require
  legacy-crm: mysql://app:secret@crm.example.com:3306/crm
  notes: sqlite:data/notes.db              # relative to this file
  local:
    host: 127.0.0.1
    database: shop
    user: shop
    allow_restore: true
```

- **Plain servers** are a connection string (`postgres://`, `mysql://`, `mariadb://` or `sqlite:`), or keys
  (`url` plus overrides, or `engine`, `host`, `port`, `database`, `user`, `ssl`). For SQLite, `database` is
  the file's path, relative to the config file; a missing file is an error, never a new empty database. A password in the file, plain text, is used as it is and kept only in memory; without one
  baselynx asks once and keeps the answer in the system keyring.
- **Editing it.** File → **Edit config file** opens the project's file in an editor that checks it as you
  type and names the line of a mistake. **Save and reload** writes only a file baselynx can read, keeps the
  previous version as `baselynx.yaml.bak`, and reloads the project.
- **Where baselynx finds it.** A `baselynx.yaml` next to the binary opens at every start; for a build under
  `target/debug` or `target/release` the repository root counts too, so `cargo run` finds it.
  `BASELYNX_CONFIG_FILE` names another file. File → **Open config file…** opens one by hand.
  `baselynx.yaml` is in `.gitignore`: it names real clusters and may hold passwords.
- **The file wins.** The databases it lists are updated from it at every start, new ones are added and ones it
  no longer lists are removed. Connections added by hand in the same project stay, and so do names and
  colours changed in the app, unless the file sets them. A project imported earlier from the same clusters is
  taken over, with its tabs.
- **Kubernetes.** baselynx opens its own port-forward through the Kubernetes API for each session; nothing
  else needs to run. Credentials are read from the secret when connecting (keys `database`, `username`,
  `password`, `connection-string`, or the names given under `keys`). Nothing from the secret is stored.
  Clusters whose name contains `prod` are red unless the file says otherwise.
- **Modes.** `read-only` makes every transaction read-only and goes through the replica service when there
  is one. The context menu switches the mode until the next start; **Open read-only** adds a read-only twin.

Connections that need no prompt connect automatically when a project opens: Kubernetes ones, ones with a
password saved in the keyring, and ones whose password was typed earlier in the session.

`crates/baselynx-core/tests/kube_live.rs` is an opt-in read-only check against a real cluster:
`BASELYNX_LIVE_CONFIG=baselynx.yaml BASELYNX_LIVE_DB="shop-prod · orders-db" cargo test -p baselynx-core --test kube_live -- --ignored`.

## Backups

A database with `backup:` in the config file is backed up when baselynx starts, if its last backup is older
than `every` (so never more often than that). **Back up now** in a connection's context menu works for any
connection. Backups run one at a time, in the background: pg_dump for PostgreSQL and mysqldump for
MySQL/MariaDB (from `backups.tools`, PATH, or on Windows the newest PostgreSQL installation), and SQLite's own
backup API for SQLite files, which needs no tools.

- Formats: `dump` (`<database>.dump`, pg_dump's compressed custom format; for SQLite, `<name>.sqlite`, a
  consistent copy of the file) and `sql` (`schema.sql`, `data.sql`, `indexes.sql`: plain SQL, readable, in the
  order a restore runs them). MySQL backups are always `sql`; `indexes.sql` holds its triggers.
- Each backup is a folder `<directory>/<cluster>/<namespace>/<name>/<UTC time>/` (`servers/<name>/…` for a
  plain server) with a `manifest.json`: source, database, versions and a SHA-256 for every file. It is
  written as `….partial` and renamed when complete; a dump is checked with `pg_restore --list` first.
- Kubernetes databases are dumped from the replica when there is one, and from the primary if that fails.
  `row_security: true` and `exclude_tables` pass through to pg_dump.
- `keep` keeps the newest `last` backups and the newest of each of the last `daily` days, `weekly` weeks and
  `monthly` months; the newest backup always stays.

**View → Backups** (or Ctrl+K, "Backups") lists every connection with its schedule, its last backup (when,
size, formats), how many backups are kept, and its state: running (with a clock), waiting, due, OK, or the
last failure with its error. Each row has **Back up now** and its folder. A failed backup is written next to
the backups as `last-failure.json` and stays until a backup of that database succeeds, so it shows after a
restart too. While backups run, the status bar says which one ("Backing up orders-db (2 of 5)"), and it
counts failed ones; both open the Backups view.

## Restoring a backup

Restores are for disasters. **Restore from backup…** in a connection's context menu asks for a file inside
the backup's folder (its `manifest.json`), then shows what goes where and every check:

1. the config file has `allow_restore: true` for this connection (the menu item is greyed out otherwise);
2. the connection is read-write;
3. the backup is complete and unchanged: every file is there and matches its checksum;
4. the backup comes from the same kind of database (a PostgreSQL backup never goes into MySQL);
5. pg_restore, psql or mysql runs (SQLite needs none);
6. the target database is empty: no tables, views, sequences, schemas besides `public`, or functions (for
   SQLite, the file holds no tables; for MySQL, the database holds no tables, routines or events).

Only when every check passes can you type the database's name and press **Restore**. A backup taken from
another database is flagged. baselynx checks everything again before it writes, restores into the primary
(never the replica), and runs the whole restore in one transaction (`pg_restore --single-transaction
--exit-on-error --no-owner`, or `psql --single-transaction -v ON_ERROR_STOP=1`), so a failed restore leaves
the database empty. SQLite restores are atomic too. MySQL commits schema changes as it goes, so a MySQL
restore that fails part-way leaves what it created: the dialog says so, and the target must be empty, so drop
it and try again.

## Menus

A menu opens when the pointer rests on its title, and the next one takes over as the pointer moves along the
bar. A menu opened that way closes when the pointer leaves it; a click on a title keeps the menu open and gives
it the keyboard, and a second click closes it.

## Where things are stored

- Projects: one JSON file per project in the configuration directory (`%APPDATA%\baselynx\config` on Windows,
  `~/Library/Application Support/baselynx` on macOS, `~/.config/baselynx` on Linux). Passwords are never written there.
- Saved passwords: the OS credential store (Windows Credential Manager, macOS Keychain, Linux Secret Service),
  service `baselynx`, account = connection id.

For testing: `BASELYNX_CONFIG_DIR` points the app at another configuration directory (and then a `baselynx.yaml` next
to the binary is not opened, unless `BASELYNX_CONFIG_FILE` names one), and `BASELYNX_THEME=light` or
`dark` pins the theme instead of following the system.

## Layout

```
crates/baselynx-core   database, storage and export; no UI dependency
crates/baselynx-app    the GPUI Kit application
fixtures/seed.sql      test data
baselynx.example.yaml  every config key, with comments
```

## Keyboard

Ctrl stands for Cmd on macOS. Every command is also in the menu bar (File, Edit, View, Query, Table), which
shows its shortcut. The sun/moon button in the title bar cycles the theme: follow the system, light, dark.
Theme and zoom are remembered.

| Where | Keys | Action |
|---|---|---|
| Anywhere | Ctrl+K | Go to a table, a row (`customers 42`), a recent query (`>`), or a command |
| | Ctrl+T | New console |
| | Ctrl+B | Focus the sidebar; again to hide it |
| | Ctrl+Shift+K | Filter the sidebar's tables |
| | Alt+[, Alt+] | Back and forward through tables reached by following keys |
| | Ctrl+W, Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+1…9 | Close, next, previous, nth tab |
| | Ctrl+=, Ctrl+-, Ctrl+0 | Zoom in, out, back to 100 % |
| Console | Ctrl+Enter | Run the statement at the caret or the selection |
| | Ctrl+Shift+Enter | Run all |
| | Tab, Enter | Take the highlighted completion (Ctrl+Space shows it) |
| | Esc | Stop |
| | Ctrl+I | Record panel for the selected result row |
| Sidebar | Up, Down, Left, Right, Enter | Move, collapse, expand, open or connect |
| | Menu key, Shift+F10 | The row's context menu |
| | typing | Starts the filter |
| Table | F5, Ctrl+R | Refresh |
| | Ctrl+F | Focus the filter (Esc in the filter clears it; Up and Down recall earlier filters) |
| | Ctrl+Shift+F, Ctrl+Shift+E | Filter by / exclude the selected value |
| | Alt+Left, Alt+Right | Previous, next page |
| | Home, End | First, last column of the row |
| | Ctrl+Home, Ctrl+End | First, last row |
| | Enter, F2, typing | Edit the cell (Enter confirms and moves down, Tab moves right, Esc cancels) |
| | Ctrl+Shift+N | Set NULL |
| | Ctrl+N, Ctrl+Delete | Add row, delete or restore row |
| | Ctrl+S | Review and commit |
| | Ctrl+Z, Ctrl+Y | Undo, redo a pending edit |
| | Ctrl+I | Record panel |
| | F4, Alt+click | Open the row a foreign key points at |
| | Space | Peek at that row (on a foreign-key cell) |
| | Shift+click, Shift+Up/Down | Select a range of cells in one column |
| | Ctrl+click | Add a cell of the same column to the selection, or take it out |
| | typing, Enter | With several cells selected, the value goes into all of them (one undo step) |
| | Ctrl+V | Paste into the selected cells, one line per cell |
| Grid | Ctrl+C | Copy the cell value, or the selected cells one per line |

## Testing the UI by hand

Never point a test run at production: start the app with `BASELYNX_CONFIG_DIR` set to a scratch directory
holding projects that only name the local test server. The local server listens on `127.0.0.1:54329`; after
stopping it, start it again with
`pg_ctl -D target/baselynx-testdb/data -l target/baselynx-testdb/server.log -o "-p 54329 -h 127.0.0.1" start`
(the data directory's own configuration does not set the port).

## Licence

Licensed under the [Apache License 2.0](LICENSE). Unless you say otherwise, any contribution you submit for
inclusion is licensed the same way, without additional terms (section 5 of the licence). Binaries you build also
contain the dependencies, under their own licences: baselynx's dependencies are under MIT, BSD, Apache-2.0, ISC,
Zlib, Unicode or the PostgreSQL licence. GPUI's font stack brings in three MPL-2.0 crates (`dwrote`, `option-ext`, and `cbindgen` as a macOS
build tool); MPL-2.0 is file-level copyleft and applies to those crates only.
