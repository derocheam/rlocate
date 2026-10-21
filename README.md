# rlocate

A fast, cross-platform `locate(1)` in Rust — a from-scratch reimplementation of
the ideas behind [`plocate`](https://plocate.sesse.net/) that runs natively on
both **macOS** and **Linux**.

Like `plocate`, `rlocate` finds files by name using a **trigram index** rather
than a linear scan, so substring, glob, and regex queries over millions of paths
return in milliseconds against a compact index.

## Workspace layout

| Crate | Kind | What it is |
|-------|------|------------|
| `rlocate-core` | lib | Index format, posting-list codec, trigrams, matcher, filename store, filesystem walk |
| `rlocate-updatedb` | bin | Builds the index by walking the filesystem (the `updatedb` equivalent) |
| `rlocate` | bin | Queries the index (the `locate` equivalent) |
| `rlocate-web` | bin | A small local web frontend (search UI over the local index + remotes) |

## Build & test

```sh
cargo build --release
cargo test
```

## Usage

```sh
# Build an index of a directory tree
rlocate-updatedb -o /tmp/test.db /usr

# Query it
rlocate -d /tmp/test.db bash             # substring
rlocate -d /tmp/test.db '*/bin/*'        # glob (anchored, * matches /)
rlocate -d /tmp/test.db -r 'lib.*\.so$'  # regex (trigram-pruned)
rlocate -d /tmp/test.db -i -c README     # case-insensitive count
rlocate -d /tmp/test.db -e -b notes.txt  # basename match, existing files only
rlocate -d /tmp/test.db -S               # index statistics
```

### Remote queries

Serve an index over the network (shared-secret token; the search runs on the
server and only matched paths are streamed back):

```sh
# on the server
rlocate --serve --listen 0.0.0.0:9999 -d /var/db/rlocate/rlocate.db --token s3cret

# from anywhere
rlocate --host server.example:9999 --token s3cret 'lib*.so'
```

The transport is plain TCP with no encryption — use it on a trusted network, or
tunnel it over SSH (`ssh -L 9999:localhost:9999 server`) and query
`--host localhost:9999`. All query flags (`-r`, `-i`, `-b`, `-e`, `-l`, `-c`,
`-0`) work remotely; `-e` is evaluated against the server's filesystem.

### Saved remotes (search several hosts automatically)

Define named remotes once in `~/.config/rlocate/remotes.conf` (override with
`--remotes-file`; it holds tokens, so `chmod 600` it):

```ini
[plex]
host = plex.local:9999
token = s3cret

[proxmox]
host = 10.0.0.5:9999
token = hunter2
```

With that file present, a plain search **automatically queries every saved
remote concurrently, plus the local index**, labeling each result by source:

```sh
$ rlocate widget
local:/home/me/widget_local.txt
plex:/srv/media/widget_report.txt
proxmox:/var/lib/widget_notes.txt

$ rlocate -c widget        # per-source counts + total
local: 1
plex: 1
proxmox: 1
total: 3
```

- `--local` searches only the local index (ignores saved remotes).
- `--host <name>` queries just one saved remote by name (uses its saved token).
- `--no-label` drops the `source:` prefix.
- A remote that is down is reported on stderr; the others still return.

To run the server on boot and refresh the index on a schedule via systemd, use
the interactive installer (it asks for the token, scan paths, frequency, and
listen address, then writes and enables all the units):

```sh
sudo ./packaging/install.sh
```

On macOS, schedule the index rebuild via launchd instead:

```sh
sudo ./packaging/install-macos.sh   # hourly by default; grant Full Disk Access
```

See [`packaging/README.md`](packaging/README.md) for the manual steps. The token
can be supplied via `--token`, the `RLOCATE_TOKEN` environment variable, or
`--token-file PATH` (which pairs with systemd encrypted credentials).

Without `-o`/`-d`, the default index path is `/var/lib/rlocate/rlocate.db`
(Linux) or `/var/db/rlocate/rlocate.db` (macOS) when run as root, else a
per-user cache path.

## Web frontend

`rlocate-web` serves a single-page search UI (and a JSON API) backed by the same
core. It searches the local index and any saved remotes together, labels each
result by source, and lets you filter results by file type and by source:

```sh
rlocate-web -d /var/db/rlocate/rlocate.db   # then open http://127.0.0.1:8080/
# --listen ADDR to change the bind address, --remotes-file PATH for remotes
```

It binds to `127.0.0.1` by default and has **no authentication** — it exposes
your index over HTTP, so keep it on localhost (don't bind a public interface).

## Design

See [`docs/design.md`](docs/design.md) for the index format and the query
pipeline. Milestones are tracked toward a performance-competitive v1 (SIMD
posting compression, trained zstd dictionaries, parallel build).

## Status

Feature-complete v1. Index build + query with substring/glob/regex,
case-insensitive (`-i`), basename (`-b`) / wholename (`-w`), existing-only
(`-e`), count (`-c`), limit (`-l`), NUL output (`-0`), and index statistics
(`-S`). Regex queries are trigram-pruned via guaranteed-literal extraction.
Performance: SIMD bitpacked posting lists, trained zstd dictionary, parallel
build, block-decompress cache. Cross-platform walk with real mount detection
and fs-type pruning on Linux and macOS (incl. APFS firmlinks). Trigram-pruned
queries over a ~200k-path index run in a few milliseconds.

Man pages: [`docs/rlocate.1`](docs/rlocate.1),
[`docs/rlocate-updatedb.1`](docs/rlocate-updatedb.1).

```sh
cargo bench -p rlocate-core   # posting codec + intersection benchmarks
```

## License

MIT OR Apache-2.0
