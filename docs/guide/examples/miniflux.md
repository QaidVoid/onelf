# Miniflux + PostgreSQL

Miniflux is a minimal, self-hosted feed reader. It ships as a static Go
binary but requires a PostgreSQL backend, which by itself is not a
single binary and brings along a pile of shared libraries, SQL bootstrap
files, extension plugins, and timezone data. That combination makes it
a good stress test for onelf.

This walkthrough produces a single `miniflux.onelf` (~70 MB) that:

- Launches a private PostgreSQL server on a Unix socket.
- Initializes the cluster on first run.
- Runs miniflux migrations and seeds an admin user.
- Starts the miniflux HTTP server at `127.0.0.1:8080`.
- Shuts PostgreSQL down cleanly on exit.

State (database cluster, postgres log, `.admin-created` sentinel) lives
under `$XDG_STATE_HOME/miniflux-onelf/` so it persists across runs.

## AppDir layout

```
miniflux-onelf/
├── onelf.toml
├── bin/
│   ├── miniflux              # static Go binary from upstream
│   ├── miniflux-wrapper      # orchestrator shell script (entrypoint)
│   ├── postgres              # postgres server binary
│   ├── initdb, pg_ctl, pg_isready, psql, createdb, createuser
├── lib/                      # bundle-libs fills this in (shared libs + loader)
│   └── postgresql/           # postgres extensions (.so), at the host's pkglibdir leaf
└── share/
    └── postgresql/           # SQL bootstrap files, tsearch dicts, timezonesets
```

## Fetching the binaries

Miniflux is a straight download:

```bash
mkdir -p miniflux-onelf/bin
curl -L -o miniflux-onelf/bin/miniflux \
    https://github.com/miniflux/v2/releases/download/2.2.19/miniflux-linux-amd64
chmod +x miniflux-onelf/bin/miniflux
```

For PostgreSQL, let `pg_config` tell you where your host keeps the
bits. It ships with any postgres install and reports the bindir,
pkglibdir, and sharedir your server was built with:

```bash
PG_BIN=$(pg_config --bindir)
PG_LIB=$(pg_config --pkglibdir)
PG_SHARE=$(pg_config --sharedir)

cp -L "$PG_BIN"/postgres     miniflux-onelf/bin/
cp -L "$PG_BIN"/initdb       miniflux-onelf/bin/
cp -L "$PG_BIN"/pg_ctl       miniflux-onelf/bin/
cp -L "$PG_BIN"/pg_isready   miniflux-onelf/bin/
cp -L "$PG_BIN"/psql         miniflux-onelf/bin/
cp -L "$PG_BIN"/createdb     miniflux-onelf/bin/
cp -L "$PG_BIN"/createuser   miniflux-onelf/bin/
chmod 755 miniflux-onelf/bin/*
```

::: tip
Always `cp -L` (or `cp -rL` for directories) so symlinks are
dereferenced to their real contents. Many distros arrange the
postgres install as a directory of symlinks pointing at versioned
paths that only exist on your machine. Packing the symlink would
ship broken references.
:::

## Bundling SQL bootstrap + extensions

PostgreSQL computes its own `sharedir` and `pkglibdir` at runtime,
relative to the server binary, using the build-time path *relationship*
between `bindir` and `pkglibdir`. So the extensions have to land in the
AppDir at the same relative position the host's postgres uses, or
`CREATE FUNCTION ... '$libdir/...'` fails at init with
`could not access file "dict_snowball"`. That relationship is distro
specific: on Debian `pkglibdir` is effectively a sibling of `bindir`'s
lib, while on Arch and Alpine it is a `postgresql` (or `postgresql<ver>`)
subdirectory of the lib dir. Derive the right target instead of hardcoding
it. Build on the same machine whose `pg_config` describes the `postgres`
binary you are bundling:

```bash
# Extensions: the .so plugins postgres dlopens for CREATE EXTENSION,
# initdb's tsearch setup, etc. Place them where the bundled postgres
# will compute $libdir: the pkglibdir path relative to its own binary.
REL=$(realpath -m --relative-to="$PG_BIN" "$PG_LIB")   # e.g. ../lib/postgresql
DEST=$(realpath -m "miniflux-onelf/bin/$REL")          # e.g. .../lib/postgresql
mkdir -p "$DEST"
cp -rL "$PG_LIB"/. "$DEST"/

# SQL files, tsearch stopwords, timezonesets, pg_hba.conf.sample, ...
mkdir -p miniflux-onelf/share
cp -rL "$PG_SHARE" miniflux-onelf/share/postgresql

chmod -R u+w miniflux-onelf/share miniflux-onelf/lib
```

Why `cp -rL` and not `cp -r`: if the source tree contains symlinks
that point outside the copied subtree (for example, into a different
install prefix, a Nix store path, or a Debian alternatives dir),
`cp -r` preserves them as-is. The resulting AppDir then ships broken
links. `-L` dereferences every symlink to its current contents.

`bundle-libs` then resolves each extension's own shared-library
dependencies into `lib/` and bakes a multi-level `$ORIGIN` RUNPATH
into every bundled ELF, so an extension under `lib/postgresql/` still
finds the bundled libraries one level up at `lib/`.

## The wrapper script

Save as `bin/miniflux-wrapper` and `chmod +x`:

```sh
#!/bin/sh
set -eu

ONELF_DIR="${ONELF_DIR:-$(cd "$(dirname "$0")/.."; pwd)}"
PGBIN="$ONELF_DIR/bin"
PGSHARE="$ONELF_DIR/share/postgresql"

DATA_ROOT="${MINIFLUX_DATA:-${XDG_STATE_HOME:-$HOME/.local/state}/miniflux-onelf}"
PGDATA="$DATA_ROOT/pgdata"
PGSOCK="$DATA_ROOT/run"
PGLOG="$DATA_ROOT/postgres.log"
mkdir -p "$DATA_ROOT" "$PGSOCK"
[ -d "$PGDATA" ] && chmod 700 "$PGDATA" 2>/dev/null || true

export PGSHAREDIR="$PGSHARE"
# Intentionally NOT exporting LD_LIBRARY_PATH. Every bundled ELF has
# DT_RUNPATH=$ORIGIN/../lib patched in by bundle-libs, so the bundled
# loader finds transitive libs via the binary's own location. Exporting
# LD_LIBRARY_PATH would leak the bundle lib dir into every child
# postgres spawns - including /bin/sh via popen(3) - which crashes on
# systems whose host glibc version differs from the bundle's.

# Point postgres at a real timezone DB on the host. The postgres
# binary has a build-time tzdata path baked in that almost never
# matches where the user's system keeps its zoneinfo.
if [ -z "${TZDIR:-}" ]; then
    for d in /usr/share/zoneinfo /etc/zoneinfo; do
        [ -d "$d" ] && export TZDIR="$d" && break
    done
fi
# Avoid the 'sh: locale: not found' noise on startup: postgres shells
# out to its build-time 'locale' binary path to enumerate locales.
export LC_ALL="${LC_ALL:-C}"
export LANG="${LANG:-C}"

pg_wait_ready() {
    i=0
    while [ $i -lt 40 ]; do
        "$PGBIN/pg_isready" -h "$PGSOCK" -U postgres >/dev/null 2>&1 && return 0
        i=$((i + 1)); sleep 0.2
    done
    return 1
}

# First-run init.
if [ ! -s "$PGDATA/PG_VERSION" ]; then
    echo "[miniflux-onelf] initializing database under $PGDATA" >&2
    "$PGBIN/initdb" --pgdata="$PGDATA" --username=postgres --auth=trust \
        --encoding=UTF8 --locale=C >/dev/null
    "$PGBIN/postgres" -D "$PGDATA" -k "$PGSOCK" -h '' \
        -c logging_collector=off -c unix_socket_permissions=0700 \
        > "$PGLOG" 2>&1 &
    PG_TMP=$!
    pg_wait_ready || { echo "postgres failed to start" >&2; exit 1; }
    "$PGBIN/createuser" -h "$PGSOCK" -U postgres -s miniflux
    "$PGBIN/createdb"   -h "$PGSOCK" -U postgres -O miniflux miniflux
    kill -TERM $PG_TMP; wait $PG_TMP 2>/dev/null || true
fi

# Start postgres for the app's lifetime.
"$PGBIN/postgres" -D "$PGDATA" -k "$PGSOCK" -h '' \
    -c logging_collector=off -c unix_socket_permissions=0700 \
    > "$PGLOG" 2>&1 &
PG_PID=$!
trap 'kill -TERM '"$PG_PID"' 2>/dev/null; wait '"$PG_PID"' 2>/dev/null; exit' \
     EXIT INT TERM
pg_wait_ready || { echo "postgres failed to become ready" >&2; exit 1; }

# Miniflux config.
export DATABASE_URL="${DATABASE_URL:-user=miniflux dbname=miniflux host=$PGSOCK sslmode=disable}"
export RUN_MIGRATIONS="${RUN_MIGRATIONS:-1}"
export LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1:8080}"
if [ ! -f "$DATA_ROOT/.admin-created" ]; then
    export CREATE_ADMIN="${CREATE_ADMIN:-1}"
    export ADMIN_USERNAME="${ADMIN_USERNAME:-admin}"
    export ADMIN_PASSWORD="${ADMIN_PASSWORD:-miniflux}"
fi

"$PGBIN/miniflux" "$@"
rc=$?
touch "$DATA_ROOT/.admin-created"
exit $rc
```

Key points:

- The wrapper reads `$ONELF_DIR` (set by the runtime) to locate its
  bundled binaries and share files. That makes it mount-path-agnostic.
- PostgreSQL listens only on a Unix socket under the state dir. No
  TCP port is opened by the database; only miniflux's HTTP server is.
- `trap ... EXIT` makes sure PostgreSQL is stopped when the wrapper
  exits (whether miniflux exits normally or gets Ctrl-C).

## The recipe

```toml
[package]
name = "miniflux"
description = "Self-hosted feed reader with an embedded PostgreSQL database"
version = "2.2.19"
homepage = "https://miniflux.app"
license = "Apache-2.0"

command = "bin/miniflux-wrapper"
working-dir = "inherit"

[bundle]
strip = true

[compression]
level = 19
```

## Building

```bash
cd miniflux-onelf
onelf bundle-libs .      # resolves libs, rewrites RUNPATH, injects bootstrap
onelf build              # produces miniflux.onelf
```

The `bundle-libs` pass prints something like:

```
Copied 32 libraries (61.6 MB) to ./lib
Rewrote RUNPATH to $ORIGIN/../lib in 194 binaries
Injected AT_EXECFN bootstrap into 9 binaries
```

`onelf build` prints the final size and the number of compressed
payload bytes.

## Running

```bash
./miniflux.onelf
```

First launch takes a couple of seconds while postgres is initialized
and miniflux runs its 125 schema migrations. Subsequent launches are
instant.

Default credentials from the recipe: `admin` / `miniflux`. Point a
browser at `http://127.0.0.1:8080` and log in.

Environment overrides:

| Variable | Default | Meaning |
|----------|---------|---------|
| `MINIFLUX_DATA` | `$XDG_STATE_HOME/miniflux-onelf` | State directory (postgres data + log) |
| `LISTEN_ADDR` | `127.0.0.1:8080` | HTTP listen address |
| `ADMIN_USERNAME` | `admin` | Seed admin username (first run only) |
| `ADMIN_PASSWORD` | `miniflux` | Seed admin password (first run only) |
| `DATABASE_URL` | (socket-based) | Override to point at a different postgres |

## Gotchas

- **Copy real files, not launcher wrappers**. Some distros install
  postgres as a thin wrapper that sets environment variables and
  then execs the real server binary. Pack the real binary, not the
  wrapper. `pg_config --bindir` usually points at whichever one
  `psql` on your host actually runs, which is the launcher on
  distros that use the wrapper pattern. If the file in `bindir` is
  suspiciously small (tens of KB instead of megabytes) it's the
  wrapper: look next to it for a hidden sibling (`.initdb-wrapped`,
  `initdb.real`, or similar) and copy that instead.
- **Match the extension directory to the host's `pkglibdir`**.
  PostgreSQL computes `$libdir` at runtime by applying the build-time
  `bindir`-to-`pkglibdir` relative path to its own binary's location.
  With `bin/postgres` at the AppDir root and `pkglibdir` a `postgresql`
  subdir of the host's lib dir (Arch, Alpine), `$libdir` resolves to
  `<mount>/lib/postgresql/`, so `dict_snowball.so`, `plpgsql.so`, and
  every other extension `.so` must live there, not flat in `lib/`. The
  `realpath --relative-to` recipe above derives the right subdirectory
  for any distro. If init dies with `could not access file
  "dict_snowball"`, the extensions are in the wrong place.
- **`share/postgresql/` must contain real files**, not symlinks.
  `cp -rL` dereferences everything.
- **tzdata and locales come from the host**. The paths postgres
  picks up for its timezone database and the `locale` binary it
  shells out to are baked in at build time and won't exist on
  another machine. The wrapper sets `TZDIR` to a host zoneinfo dir
  and `LC_ALL=C` so postgres skips the locale enumeration. That
  silences the warnings without having to bundle 5 MB of zoneinfo or
  a glibc locale binary.
