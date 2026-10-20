//! Query execution: turn a [`Query`] + [`Index`] into matching paths.
//!
//! Strategy: decompose the query into required trigrams, intersect their
//! posting lists to a candidate set, then verify each candidate against the
//! real matcher. Queries that cannot produce trigrams fall back to scanning
//! every filename block.

use crate::index::Index;
use crate::matcher::Query;
use crate::trigram::QueryTrigrams;
use crate::Result;
use crate::BLOCK_SIZE;

/// Run `query` against `index`, invoking `emit` for each matching path (raw
/// bytes), stopping early once `emit` returns `false`.
///
/// `emit` returning `false` means "enough results" (used to honour `--limit`).
pub fn search<F>(index: &Index, query: &Query, mut emit: F) -> Result<()>
where
    F: FnMut(&[u8]) -> bool,
{
    let names = index.names()?;
    match query.trigrams() {
        QueryTrigrams::FullScan => {
            for b in 0..names.block_count() {
                for path in names.block(b)? {
                    if query.is_match(&path) && !emit(&path) {
                        return Ok(());
                    }
                }
            }
        }
        QueryTrigrams::Required(tris) => {
            // Gather posting lists; a missing trigram means no possible match.
            let mut lists = Vec::with_capacity(tris.len());
            for t in tris {
                match index.posting_list(t)? {
                    Some(list) => lists.push(list),
                    None => return Ok(()),
                }
            }
            let candidates = crate::postings::intersect(lists);
            // Candidates are sorted, so consecutive docids usually fall in the
            // same block; decompress each block at most once.
            let mut cached_block: Option<(usize, Vec<Vec<u8>>)> = None;
            for docid in candidates {
                let block_no = docid as usize / BLOCK_SIZE;
                let within = docid as usize % BLOCK_SIZE;
                if cached_block.as_ref().map(|(n, _)| *n) != Some(block_no) {
                    cached_block = Some((block_no, names.block(block_no)?));
                }
                let block = &cached_block.as_ref().unwrap().1;
                let path = block
                    .get(within)
                    .ok_or(crate::Error::Corrupt("docid not found in block"))?;
                if query.is_match(path) && !emit(path) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Convenience wrapper collecting up to `limit` matches (0 = unlimited).
pub fn search_collect(index: &Index, query: &Query, limit: usize) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    search(index, query, |p| {
        out.push(p.to_vec());
        limit == 0 || out.len() < limit
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{MatchKind, Options, Query};
    use crate::names::NameStoreBuilder;
    use crate::trigram;
    use std::collections::BTreeMap;

    fn make_index(paths: &[&[u8]]) -> (tempfile::NamedTempFile, Index) {
        let mut nb = NameStoreBuilder::new(19);
        let mut postings: BTreeMap<trigram::Trigram, Vec<u32>> = BTreeMap::new();
        let mut sorted: Vec<&[u8]> = paths.to_vec();
        sorted.sort_unstable();
        for (docid, p) in sorted.iter().enumerate() {
            nb.push(p);
            for t in trigram::of_path(p) {
                postings.entry(t).or_default().push(docid as u32);
            }
        }
        let ns = nb.finish();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut bytes = Vec::new();
        crate::index::write(&mut bytes, sorted.len() as u32, &postings, &ns).unwrap();
        std::io::Write::write_all(&mut tmp.as_file(), &bytes).unwrap();
        let idx = Index::open(tmp.path()).unwrap();
        (tmp, idx)
    }

    /// Brute-force oracle used to validate the indexed search.
    fn oracle(paths: &[&[u8]], q: &Query) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = paths
            .iter()
            .filter(|p| q.is_match(p))
            .map(|p| p.to_vec())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn indexed_matches_oracle() {
        let paths: Vec<&[u8]> = vec![
            b"/usr/bin/bash",
            b"/usr/bin/zsh",
            b"/usr/bin/bat",
            b"/etc/hosts",
            b"/usr/lib/libc.so",
            b"/home/user/notes.txt",
            b"/var/log/syslog",
        ];
        let (_tmp, idx) = make_index(&paths);

        for (pat, kind) in [
            ("bash", MatchKind::Substring),
            ("bin", MatchKind::Substring),
            ("ba", MatchKind::Substring),  // short -> full scan
            ("xyz", MatchKind::Substring), // no matches
            ("*/bin/*", MatchKind::Glob),
            ("lib*so", MatchKind::Glob),
            (r"ba(sh|t)", MatchKind::Regex),
        ] {
            let q = Query::new(pat, kind, Options::default()).unwrap();
            let mut got = search_collect(&idx, &q, 0).unwrap();
            got.sort();
            assert_eq!(got, oracle(&paths, &q), "pattern {pat:?}");
        }
    }

    #[test]
    fn limit_is_respected() {
        let paths: Vec<&[u8]> = vec![b"/a/one", b"/a/two", b"/a/three", b"/a/four"];
        let (_tmp, idx) = make_index(&paths);
        let q = Query::new("/a/", MatchKind::Substring, Options::default()).unwrap();
        let got = search_collect(&idx, &q, 2).unwrap();
        assert_eq!(got.len(), 2);
    }
}
