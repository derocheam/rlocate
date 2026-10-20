//! Pattern matching: substring, glob, and regex, with basename and
//! case-insensitive variants.
//!
//! Matching operates on raw path bytes. A [`Query`] both decides whether a
//! candidate path matches and reports the trigrams that every match must
//! contain (used to prune candidates via the index).

use crate::trigram::{self, QueryTrigrams, Trigram};
use regex::bytes::Regex;
use std::collections::BTreeSet;

/// How the pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Match anywhere in the path (GNU locate default).
    Substring,
    /// `fnmatch`-style glob, anchored to the whole string.
    Glob,
    /// Regular expression, unanchored (matches anywhere).
    Regex,
}

/// Options influencing interpretation and matching.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub ignore_case: bool,
    /// Match only against the basename (final path component).
    pub basename: bool,
}

/// A compiled query ready to test candidates and emit required trigrams.
pub struct Query {
    opts: Options,
    engine: Engine,
    /// Literal segments the pattern requires, used for trigram pruning.
    literals: Vec<Vec<u8>>,
    case_sensitive_trigrams: bool,
}

enum Engine {
    Substring(Vec<u8>),
    Regex(Regex),
}

/// Detect whether `pattern` should be treated as a glob (contains `* ? [`).
pub fn looks_like_glob(pattern: &str) -> bool {
    pattern.bytes().any(|b| matches!(b, b'*' | b'?' | b'['))
}

impl Query {
    /// Build a query from a pattern string, an explicit kind, and options.
    pub fn new(pattern: &str, kind: MatchKind, opts: Options) -> crate::Result<Self> {
        match kind {
            MatchKind::Substring => {
                let needle = pattern.as_bytes().to_vec();
                let literals = vec![needle.clone()];
                let engine = if opts.ignore_case {
                    Engine::Regex(build_regex(&regex::escape(pattern), true)?)
                } else {
                    Engine::Substring(needle)
                };
                Ok(Self {
                    opts,
                    engine,
                    literals,
                    case_sensitive_trigrams: !opts.ignore_case,
                })
            }
            MatchKind::Glob => {
                let re_src = glob_to_regex(pattern);
                let engine = Engine::Regex(build_regex(&re_src, opts.ignore_case)?);
                let literals = glob_literal_segments(pattern);
                Ok(Self {
                    opts,
                    engine,
                    literals,
                    case_sensitive_trigrams: !opts.ignore_case,
                })
            }
            MatchKind::Regex => {
                let engine = Engine::Regex(build_regex(pattern, opts.ignore_case)?);
                // Any match must contain the regex's guaranteed literal runs, so
                // their trigrams can prune candidates. If none qualify, the
                // `trigrams()` call falls back to a full scan.
                let literals = regex_required_literals(pattern);
                Ok(Self {
                    opts,
                    engine,
                    literals,
                    case_sensitive_trigrams: !opts.ignore_case,
                })
            }
        }
    }

    /// The trigrams every matching path must contain, or [`QueryTrigrams::FullScan`].
    pub fn trigrams(&self) -> QueryTrigrams {
        if !self.case_sensitive_trigrams {
            return QueryTrigrams::FullScan;
        }
        let mut set: BTreeSet<Trigram> = BTreeSet::new();
        for lit in &self.literals {
            trigram::extract(lit, &mut set);
        }
        if set.is_empty() {
            QueryTrigrams::FullScan
        } else {
            QueryTrigrams::Required(set.into_iter().collect())
        }
    }

    /// Does `path` match this query?
    pub fn is_match(&self, path: &[u8]) -> bool {
        let subject = if self.opts.basename {
            basename(path)
        } else {
            path
        };
        match &self.engine {
            Engine::Substring(needle) => memchr::memmem::find(subject, needle).is_some(),
            Engine::Regex(re) => re.is_match(subject),
        }
    }
}

/// The final path component (after the last `/`).
pub fn basename(path: &[u8]) -> &[u8] {
    match memchr::memrchr(b'/', path) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn build_regex(src: &str, ignore_case: bool) -> crate::Result<Regex> {
    regex::bytes::RegexBuilder::new(src)
        .case_insensitive(ignore_case)
        .unicode(false) // operate on raw bytes
        .build()
        .map_err(|e| crate::Error::Pattern(e.to_string()))
}

/// Translate an `fnmatch`-style glob into an anchored byte regex.
///
/// `*` matches any run (including `/`), `?` any single byte, `[...]` a class.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::with_capacity(glob.len() + 4);
    re.push('^');
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                re.push('[');
                // Copy class body verbatim until the matching ']'.
                if matches!(chars.peek(), Some('!') | Some('^')) {
                    chars.next();
                    re.push('^');
                }
                for cc in chars.by_ref() {
                    re.push(cc);
                    if cc == ']' {
                        break;
                    }
                }
            }
            // Escape regex metacharacters that are literal in globs.
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}

/// Extract the maximal runs of *mandatory* literal bytes from a regex.
///
/// A match of the regex must contain each returned run as a contiguous
/// substring, so their trigrams are valid pruning constraints. Constructs that
/// do not guarantee a fixed literal (classes, alternation, repetition, anchors)
/// break the current run and contribute nothing — a sound under-approximation.
fn regex_required_literals(pattern: &str) -> Vec<Vec<u8>> {
    use regex_syntax::hir::HirKind;

    let Ok(hir) = regex_syntax::ParserBuilder::new()
        .unicode(false)
        .utf8(false)
        .build()
        .parse(pattern)
    else {
        return Vec::new();
    };

    fn walk(hir: &regex_syntax::hir::Hir, cur: &mut Vec<u8>, runs: &mut Vec<Vec<u8>>) {
        match hir.kind() {
            HirKind::Literal(lit) => cur.extend_from_slice(&lit.0),
            HirKind::Concat(parts) => {
                for p in parts {
                    walk(p, cur, runs);
                }
            }
            HirKind::Capture(cap) => walk(&cap.sub, cur, runs),
            // Everything else (Empty, Class, Look, Repetition, Alternation)
            // breaks the guaranteed-literal run.
            _ => {
                if !cur.is_empty() {
                    runs.push(std::mem::take(cur));
                }
            }
        }
    }

    let mut runs = Vec::new();
    let mut cur = Vec::new();
    walk(&hir, &mut cur, &mut runs);
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// Extract the literal (wildcard-free) runs of a glob for trigram pruning.
fn glob_literal_segments(glob: &str) -> Vec<Vec<u8>> {
    let mut segs = Vec::new();
    let mut cur = Vec::new();
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' => {
                if !cur.is_empty() {
                    segs.push(std::mem::take(&mut cur));
                }
            }
            '[' => {
                // Skip the whole class; it is not a fixed literal.
                if !cur.is_empty() {
                    segs.push(std::mem::take(&mut cur));
                }
                for cc in chars.by_ref() {
                    if cc == ']' {
                        break;
                    }
                }
            }
            _ => {
                let mut buf = [0u8; 4];
                cur.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_contains() {
        let q = Query::new("bin", MatchKind::Substring, Options::default()).unwrap();
        assert!(q.is_match(b"/usr/bin/bash"));
        assert!(!q.is_match(b"/etc/hosts"));
    }

    #[test]
    fn glob_anchored_whole_path() {
        let q = Query::new("*/bin/*", MatchKind::Glob, Options::default()).unwrap();
        assert!(q.is_match(b"/usr/bin/bash"));
        assert!(!q.is_match(b"/usr/sbin/ip")); // "/bin/" not present
    }

    #[test]
    fn glob_star_matches_slash() {
        let q = Query::new("/usr*bash", MatchKind::Glob, Options::default()).unwrap();
        assert!(q.is_match(b"/usr/bin/bash"));
    }

    #[test]
    fn regex_unanchored() {
        let q = Query::new(r"ba(sh|t)", MatchKind::Regex, Options::default()).unwrap();
        assert!(q.is_match(b"/usr/bin/bash"));
        assert!(q.is_match(b"/usr/bin/bat"));
        assert!(!q.is_match(b"/usr/bin/zsh"));
    }

    #[test]
    fn basename_only() {
        let opts = Options {
            ignore_case: false,
            basename: true,
        };
        let q = Query::new("usr", MatchKind::Substring, opts).unwrap();
        assert!(!q.is_match(b"/usr/bin/bash")); // "usr" only in dir, not basename
        assert!(q.is_match(b"/home/usr"));
    }

    #[test]
    fn ignore_case_forces_fullscan() {
        let q = Query::new(
            "Bash",
            MatchKind::Substring,
            Options {
                ignore_case: true,
                basename: false,
            },
        )
        .unwrap();
        assert_eq!(q.trigrams(), QueryTrigrams::FullScan);
        assert!(q.is_match(b"/usr/bin/bash"));
    }

    #[test]
    fn glob_literal_segments_extracted() {
        assert_eq!(
            glob_literal_segments("*foo*bar?baz"),
            vec![b"foo".to_vec(), b"bar".to_vec(), b"baz".to_vec()]
        );
    }

    #[test]
    fn regex_required_literals_extracted() {
        // Literal runs separated by `.*` and a char class.
        assert_eq!(
            regex_required_literals(r"lib.*\.dylib$"),
            vec![b"lib".to_vec(), b".dylib".to_vec()]
        );
        // Alternation yields no guaranteed literal past the common prefix.
        assert_eq!(regex_required_literals(r"ba(sh|t)"), vec![b"ba".to_vec()]);
        // A pure alternation has no mandatory literal at all.
        assert!(regex_required_literals(r"(foo|bar)").is_empty());
    }

    #[test]
    fn regex_uses_trigrams_when_possible() {
        let q = Query::new(r"lib.*\.dylib$", MatchKind::Regex, Options::default()).unwrap();
        match q.trigrams() {
            QueryTrigrams::Required(t) => assert!(!t.is_empty()),
            _ => panic!("expected regex to produce required trigrams"),
        }
        // Short/absent literals fall back to a full scan.
        assert_eq!(
            Query::new(r"ba(sh|t)", MatchKind::Regex, Options::default())
                .unwrap()
                .trigrams(),
            QueryTrigrams::FullScan
        );
    }

    #[test]
    fn substring_trigrams_required() {
        let q = Query::new("bash", MatchKind::Substring, Options::default()).unwrap();
        match q.trigrams() {
            QueryTrigrams::Required(t) => assert_eq!(t.len(), 2), // "bas","ash"
            _ => panic!("expected required trigrams"),
        }
    }
}
