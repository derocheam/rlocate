# rlocate design

`rlocate` is a cross-platform (macOS + Linux) reimplementation of the ideas
behind [`plocate`](https://plocate.sesse.net/): find files by name fast using a
**trigram index** rather than a linear scan.

## Concepts

- **Document** — one path. Paths are sorted bytewise; a path's position in that
  order is its **docid** (`u32`).
- **Trigram** — a 3-byte window over a path, packed into the low 24 bits of a
  `u32`. Every distinct trigram in a path maps to that path's docid.
- **Posting list** — for one trigram, the sorted docids of every path that
  contains it.

## Query pipeline

1. Decide the match kind: substring (default), glob (`* ? [` present), or regex
   (`-r`). See [`matcher.rs`](../crates/rlocate-core/src/matcher.rs).
2. Decompose the pattern into the trigrams every match must contain.
   - Substring: trigrams of the needle.
   - Glob: trigrams of each wildcard-free literal run.
   - Regex: trigrams of the regex's *guaranteed* literal runs, extracted from
     the parsed HIR (e.g. `lib.*\.dylib$` requires `lib` and `.dylib`).
   - Case-insensitive / no literal run ≥ 3 bytes: no usable trigrams → **full
     scan** of all filename blocks.
3. Look up each trigram's posting list and **intersect** them (shortest first).
   A missing trigram ⇒ empty result.
4. For each candidate docid, decompress its filename and **verify** against the
   real matcher. See [`search.rs`](../crates/rlocate-core/src/search.rs).

## On-disk format (`rlocate.db`)

Little-endian, mmap-friendly. See [`index.rs`](../crates/rlocate-core/src/index.rs).

```text
[Header]            108 bytes: magic "RLOC", version, flags, counts, region table
[Trigram table]     num_trigrams × 16 bytes, sorted: u32 trigram | u32 count | u64 posting_off
[Posting region]    per-trigram posting lists
[Name dictionary]   zstd dictionary (empty until M3)
[Name block index]  (num_blocks + 1) × u64 offsets into the name blocks
[Name blocks]       zstd-compressed, NUL-separated, 32 paths per block
```

- Trigram lookup: binary search the sorted table.
- Filename lookup: `docid / 32` → block number → `block_index[n]` → decompress →
  pick entry `docid % 32`.

### Posting-list codec

Full groups of 128 docids are **delta-bitpacked** with the SIMD `bitpacking`
codec (Lemire `BitPacker4x`, the Rust analogue of plocate's TurboPFor): one
`num_bits` byte then `num_bits * 16` packed bytes per block. The trailing
`< 128` docids use delta + LEB128 varint, which is also the whole encoding for
short lists (the common case). All isolated in
[`postings.rs`](../crates/rlocate-core/src/postings.rs) behind `encode`/`decode`.

Measured (M3, Apple Silicon): decode ≈ 39 µs for 100k docids (~2.5 B docids/s);
rare×common intersection ≈ 11 µs. End to end, trigram-pruned queries over a
196k-path index run in a few ms vs ~1.6 s for `find | grep`.

## Build pipeline

[`rlocate-updatedb`](../crates/rlocate-updatedb/src/main.rs):

1. Walk roots ([`fs/mod.rs`](../crates/rlocate-core/src/fs/mod.rs)); no symlink
   following; prune by name/path; stay on one filesystem by device id.
2. Sort + dedup paths (assigns docids).
3. Build the filename store and trigram postings.
4. Write to a temp file, `fsync`, atomic `rename` into place.

## Cross-platform notes

- The walk crosses mount boundaries by default (like mlocate/plocate), detecting
  them via `MetadataExt::dev()` changes. At each boundary it looks up the
  filesystem type in a [`Mounts`](../crates/rlocate-core/src/fs/mod.rs) table —
  `/proc/self/mounts` on Linux ([`fs/linux.rs`](../crates/rlocate-core/src/fs/linux.rs)),
  `getmntinfo(3)` on macOS ([`fs/macos.rs`](../crates/rlocate-core/src/fs/macos.rs)) —
  and prunes network/pseudo types (`nfs`, `smbfs`, `devfs`, `proc`, …).
  `-x/--one-file-system` restricts the walk to the root's filesystem.
- **APFS**: `/Users`, `/Applications`, etc. are firmlinks into the Data volume,
  so the whole volume is reachable from `/`. The Data volume's direct mountpoint
  (`/System/Volumes/Data`) is pruned by default to avoid walking it twice.
- Prune defaults differ per OS; see
  [`config.rs`](../crates/rlocate-core/src/config.rs).
- APFS is usually case-insensitive; indexing stays byte-exact and `-i` is the
  user's tool for case folding.
- A root-built system index is installed atomically (temp file + `rename`) and
  made world-readable (`0644`) so unprivileged users can query it.

## Remote queries

The `rlocate` binary has these modes:

- **local** (default when no remotes are configured, or with `--local`) — open
  the mmapped index and search.
- **`--serve`** — load the index once (`Arc<Index>`, shared across threads) and
  accept TCP connections, one thread per client.
- **`--host ADDR|NAME`** — query a single remote. `NAME` resolves against the
  saved-remotes config ([`remotes.rs`](../crates/rlocate-core/src/remotes.rs))
  to supply host + token.
- **aggregate** (default when a remotes config with entries exists) — search the
  local index and every saved remote concurrently (one thread per remote),
  labeling each result by source. A down remote is reported on stderr and
  skipped; the rest still return. `--local` opts out.

The wire protocol lives in [`net.rs`](../crates/rlocate-core/src/net.rs) so
client and server share one definition:

```text
Request:  "RLQ1" | token | flags | limit | patterns…
Response: "RLR1" | status | (len:u32 | path)*  ending in len == 0xFFFF_FFFF
```

The search executes on the server (via the same `execute` path as local mode),
so only matched paths cross the wire and `limit`/`-e` are applied server-side
(`-e` tests the server's filesystem). Authentication is a shared-secret
`--token`, compared in constant time. The transport is **plain, unencrypted
TCP** — meant for a trusted network or an SSH tunnel, not a hostile one; a
network attacker can read paths and the token. TLS/real auth is a future option.

## Milestones

| | Status | Scope |
|---|---|---|
| M0 | ✅ | Toolchain, workspace, format, CI |
| M1 | ✅ | Indexer MVP |
| M2 | ✅ | Searcher MVP (substring/glob/regex, `-i/-b/-c/-l/-0`) |
| M3 | ✅ | SIMD bitpacked postings, trained zstd dict, parallel build, block cache, benchmarks |
| M4 | ✅ | Mount detection (`/proc/mounts`, `getmntinfo`), fs-type pruning, APFS firmlinks, atomic install + perms |
| M5 | ✅ | GNU flags (`-e -w -S`), regex trigram extraction, man pages |
