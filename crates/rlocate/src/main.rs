//! `rlocate` — search a local index, saved remotes, or an explicit host.
//!
//! Modes (first match wins):
//! * `--serve` — run as a query server.
//! * `--host ADDR|NAME` — query one remote (NAME is resolved from the saved
//!   remotes config, supplying its host and token).
//! * otherwise — if a remotes config with entries exists and `--local` is not
//!   given, search the local index *and* every saved remote concurrently,
//!   labeling each result by source. If there is no config (or `--local`), do a
//!   plain local search.

use anyhow::{bail, Context, Result};
use clap::Parser;
use rlocate_core::index::Index;
use rlocate_core::matcher::{looks_like_glob, MatchKind, Options, Query};
use rlocate_core::remotes::{self, Remote};
use rlocate_core::{net, search};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

/// Find files by name using a prebuilt trigram index.
#[derive(Parser, Debug)]
#[command(name = "rlocate", version, about)]
struct Args {
    /// Pattern(s) to search for. Optional only with --statistics or --serve.
    #[arg(value_name = "PATTERN")]
    patterns: Vec<String>,

    /// Use the index at this path (default: platform db path).
    #[arg(short = 'd', long = "database", value_name = "PATH")]
    database: Option<PathBuf>,

    /// Interpret patterns as basic regular expressions.
    #[arg(short = 'r', long = "regexp")]
    regexp: bool,

    /// Case-insensitive matching.
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Match only the base name of the path.
    #[arg(short = 'b', long = "basename")]
    basename: bool,

    /// Match against the whole path name (the default; overrides -b).
    #[arg(short = 'w', long = "wholename")]
    wholename: bool,

    /// Only print entries that still exist on disk (checked on the server when
    /// querying remotely).
    #[arg(short = 'e', long = "existing")]
    existing: bool,

    /// Print only the number of matches.
    #[arg(short = 'c', long = "count")]
    count: bool,

    /// Limit output to this many matches (0 = unlimited; per source when
    /// searching several).
    #[arg(short = 'l', long = "limit", visible_alias = "n", default_value_t = 0)]
    limit: usize,

    /// Separate results with NUL instead of newline.
    #[arg(short = '0', long = "null")]
    null: bool,

    /// Print statistics about the local index and exit.
    #[arg(short = 'S', long = "statistics")]
    statistics: bool,

    /// Search only the local index, ignoring any saved remotes.
    #[arg(long = "local", conflicts_with = "host")]
    local: bool,

    /// Do not prefix results with their source name in a multi-source search.
    #[arg(long = "no-label")]
    no_label: bool,

    /// Use this remotes config file instead of the default.
    #[arg(long = "remotes-file", value_name = "PATH")]
    remotes_file: Option<PathBuf>,

    /// Run as a query server instead of searching.
    #[arg(long = "serve", conflicts_with = "host")]
    serve: bool,

    /// Address to listen on in --serve mode.
    #[arg(long = "listen", value_name = "ADDR", default_value = "127.0.0.1:9999")]
    listen: String,

    /// Query a remote server: either a host:port or the name of a saved remote.
    #[arg(long = "host", value_name = "ADDR|NAME")]
    host: Option<String>,

    /// Shared secret: required by --serve, and sent with each --host query
    /// (overrides a saved remote's token).
    #[arg(long = "token", value_name = "TOKEN")]
    token: Option<String>,
}

/// Matching parameters shared by local, server, and remote execution.
#[derive(Clone, Copy)]
struct QueryParams {
    regexp: bool,
    ignore_case: bool,
    basename: bool,
    existing: bool,
    limit: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(found) => {
            if found {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1) // GNU locate exits 1 when nothing matched.
            }
        }
        Err(e) => {
            eprintln!("rlocate: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse();

    if args.serve {
        return serve(&args);
    }
    if let Some(host) = args.host.clone() {
        if args.patterns.is_empty() {
            bail!("no pattern given");
        }
        return single_remote(&args, &host);
    }
    // Statistics always refers to the local index.
    if args.statistics {
        return local_search(&args);
    }
    // Auto: search saved remotes (+ local) when a config with entries exists.
    if !args.local {
        let remotes = load_remotes(&args)?;
        if !remotes.is_empty() {
            if args.patterns.is_empty() {
                bail!("no pattern given");
            }
            return aggregate_search(&args, remotes);
        }
    }
    local_search(&args)
}

impl Args {
    fn query_params(&self) -> QueryParams {
        QueryParams {
            regexp: self.regexp,
            ignore_case: self.ignore_case,
            basename: self.basename && !self.wholename, // -w overrides -b
            existing: self.existing,
            limit: self.limit,
        }
    }
}

// ---------------------------------------------------------------------------
// Local search
// ---------------------------------------------------------------------------

fn local_search(args: &Args) -> Result<bool> {
    if args.patterns.is_empty() && !args.statistics {
        bail!("no pattern given (use --statistics to inspect the index)");
    }
    let db_path = args
        .database
        .clone()
        .unwrap_or_else(rlocate_core::default_db_path);
    let index =
        Index::open(&db_path).with_context(|| format!("opening index {}", db_path.display()))?;

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    if args.statistics {
        print_statistics(&mut out, &index, &db_path)?;
        if args.patterns.is_empty() {
            out.flush()?;
            return Ok(true);
        }
    }

    let params = args.query_params();
    let sep = if args.null { 0u8 } else { b'\n' };
    let total = if args.count {
        execute(&index, &args.patterns, &params, |_| Ok(()))?
    } else {
        execute(&index, &args.patterns, &params, |path| {
            out.write_all(path)?;
            out.write_all(&[sep])
        })?
    };
    if args.count {
        writeln!(out, "{total}")?;
    }
    out.flush()?;
    Ok(total > 0)
}

/// Collect local matches into a vector (for an aggregate search). Returns
/// `None` if there is no local index to search.
fn local_collect(args: &Args) -> Result<Option<Vec<Vec<u8>>>> {
    let db_path = args
        .database
        .clone()
        .unwrap_or_else(rlocate_core::default_db_path);
    if !db_path.exists() {
        return Ok(None);
    }
    let index =
        Index::open(&db_path).with_context(|| format!("opening index {}", db_path.display()))?;
    let mut out = Vec::new();
    execute(&index, &args.patterns, &args.query_params(), |path| {
        out.push(path.to_vec());
        Ok(())
    })?;
    Ok(Some(out))
}

/// Run every pattern against `index`, invoking `on_match` per surviving match
/// (after the `existing` filter), honouring `limit`. Returns the match count.
fn execute(
    index: &Index,
    patterns: &[String],
    p: &QueryParams,
    mut on_match: impl FnMut(&[u8]) -> io::Result<()>,
) -> Result<usize> {
    let opts = Options {
        ignore_case: p.ignore_case,
        basename: p.basename,
    };
    let mut total = 0usize;
    let mut io_err: Option<io::Error> = None;

    for pattern in patterns {
        let kind = if p.regexp {
            MatchKind::Regex
        } else if looks_like_glob(pattern) {
            MatchKind::Glob
        } else {
            MatchKind::Substring
        };
        let query = Query::new(pattern, kind, opts)?;

        search::search(index, &query, |path| {
            if p.existing && !exists(path) {
                return true;
            }
            total += 1;
            if let Err(e) = on_match(path) {
                io_err = Some(e);
                return false;
            }
            p.limit == 0 || total < p.limit
        })?;

        if io_err.is_some() || (p.limit != 0 && total >= p.limit) {
            break;
        }
    }

    if let Some(e) = io_err {
        return Err(e.into());
    }
    Ok(total)
}

fn print_statistics<W: Write>(out: &mut W, index: &Index, db_path: &Path) -> Result<()> {
    let size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    writeln!(out, "Index {}:", db_path.display())?;
    writeln!(out, "\t{} paths", index.num_docs())?;
    writeln!(out, "\t{} distinct trigrams", index.num_trigrams())?;
    writeln!(out, "\t{} filename blocks", index.num_blocks())?;
    writeln!(out, "\t{size} bytes on disk")?;
    if index.num_docs() > 0 {
        writeln!(
            out,
            "\t{:.1} bytes per path",
            size as f64 / f64::from(index.num_docs())
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Remotes
// ---------------------------------------------------------------------------

fn load_remotes(args: &Args) -> Result<Vec<Remote>> {
    let path = args.remotes_file.clone().or_else(remotes::default_path);
    match path {
        Some(p) => remotes::load(&p).with_context(|| format!("reading {}", p.display())),
        None => Ok(Vec::new()),
    }
}

fn build_request(token: String, p: &QueryParams, patterns: &[String]) -> net::Request {
    net::Request {
        token,
        regexp: p.regexp,
        ignore_case: p.ignore_case,
        basename: p.basename,
        existing: p.existing,
        limit: p.limit as u32,
        patterns: patterns.to_vec(),
    }
}

/// Connect to `addr`, send `request`, and invoke `on_path` for each streamed
/// result path.
fn remote_query(
    addr: &str,
    request: &net::Request,
    mut on_path: impl FnMut(&[u8]) -> io::Result<()>,
) -> Result<()> {
    let stream = TcpStream::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream); // kept alive until the response is read
    request.write_to(&mut writer)?;

    match net::read_status(&mut reader)? {
        net::STATUS_OK => {}
        net::STATUS_AUTH_FAILED => bail!("{}", net::read_message(&mut reader)?),
        _ => bail!("{}", net::read_message(&mut reader)?),
    }
    while let Some(path) = net::read_result(&mut reader)? {
        on_path(&path)?;
    }
    Ok(())
}

/// Query one remote (host:port or a saved-remote NAME) and print results.
fn single_remote(args: &Args, host: &str) -> Result<bool> {
    let (addr, token) = resolve_host(args, host);
    let request = build_request(token, &args.query_params(), &args.patterns);

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let sep = if args.null { 0u8 } else { b'\n' };
    let mut total = 0usize;
    remote_query(&addr, &request, |path| {
        total += 1;
        if !args.count {
            out.write_all(path)?;
            out.write_all(&[sep])?;
        }
        Ok(())
    })?;
    if args.count {
        writeln!(out, "{total}")?;
    }
    out.flush()?;
    Ok(total > 0)
}

/// Resolve a `--host` value: a saved-remote name supplies host+token; otherwise
/// it is treated as a literal host:port with the `--token` (if any).
fn resolve_host(args: &Args, host: &str) -> (String, String) {
    let remotes = load_remotes(args).unwrap_or_default();
    if let Some(r) = remotes.iter().find(|r| r.name == host) {
        let token = args.token.clone().unwrap_or_else(|| r.token.clone());
        (r.host.clone(), token)
    } else {
        (host.to_string(), args.token.clone().unwrap_or_default())
    }
}

/// A named in-flight remote query returning its matched paths.
type RemoteHandle = (String, thread::JoinHandle<Result<Vec<Vec<u8>>>>);

/// Search the local index and all saved remotes concurrently, labeling results.
fn aggregate_search(args: &Args, remotes: Vec<Remote>) -> Result<bool> {
    let params = args.query_params();

    // Fire off every remote in its own thread.
    let handles: Vec<RemoteHandle> = remotes
        .into_iter()
        .map(|r| {
            let request = build_request(r.token, &params, &args.patterns);
            let addr = r.host;
            let handle = thread::spawn(move || {
                let mut v = Vec::new();
                remote_query(&addr, &request, |p| {
                    v.push(p.to_vec());
                    Ok(())
                })?;
                Ok(v)
            });
            (r.name, handle)
        })
        .collect();

    // Search the local index while remotes are in flight.
    let local = match local_collect(args) {
        Ok(opt) => opt,
        Err(e) => {
            eprintln!("rlocate: local: {e:#}");
            None
        }
    };

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let label = !args.no_label;
    let sep = if args.null { 0u8 } else { b'\n' };
    let mut total = 0usize;

    if let Some(paths) = local {
        total += print_source(&mut out, "local", &paths, args.count, label, sep)?;
    }
    for (name, handle) in handles {
        match handle.join() {
            Ok(Ok(paths)) => {
                total += print_source(&mut out, &name, &paths, args.count, label, sep)?
            }
            Ok(Err(e)) => eprintln!("rlocate: {name}: {e:#}"),
            Err(_) => eprintln!("rlocate: {name}: worker thread panicked"),
        }
    }
    if args.count {
        writeln!(out, "total: {total}")?;
    }
    out.flush()?;
    Ok(total > 0)
}

fn print_source<W: Write>(
    out: &mut W,
    name: &str,
    paths: &[Vec<u8>],
    count: bool,
    label: bool,
    sep: u8,
) -> io::Result<usize> {
    if count {
        writeln!(out, "{name}: {}", paths.len())?;
    } else {
        for p in paths {
            if label {
                out.write_all(name.as_bytes())?;
                out.write_all(b":")?;
            }
            out.write_all(p)?;
            out.write_all(&[sep])?;
        }
    }
    Ok(paths.len())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

fn serve(args: &Args) -> Result<bool> {
    let token = args.token.clone().context("--serve requires --token")?;
    if token.is_empty() {
        bail!("--token must not be empty");
    }
    let db_path = args
        .database
        .clone()
        .unwrap_or_else(rlocate_core::default_db_path);
    let index = Arc::new(
        Index::open(&db_path).with_context(|| format!("opening index {}", db_path.display()))?,
    );
    let listener =
        TcpListener::bind(&args.listen).with_context(|| format!("binding {}", args.listen))?;
    eprintln!(
        "rlocate: serving {} ({} paths) on {}",
        db_path.display(),
        index.num_docs(),
        args.listen
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let index = Arc::clone(&index);
                let token = token.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &index, &token) {
                        eprintln!("rlocate: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("rlocate: accept error: {e}"),
        }
    }
    Ok(true)
}

fn handle_conn(stream: TcpStream, index: &Index, token: &str) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let req = match net::Request::read_from(&mut reader) {
        Ok(req) => req,
        Err(e) => {
            net::write_status(&mut writer, net::STATUS_ERROR)?;
            net::write_message(&mut writer, &format!("bad request: {e}"))?;
            writer.flush()?;
            return Ok(());
        }
    };

    if !net::token_eq(&req.token, token) {
        net::write_status(&mut writer, net::STATUS_AUTH_FAILED)?;
        net::write_message(&mut writer, "authentication failed")?;
        writer.flush()?;
        return Ok(());
    }

    net::write_status(&mut writer, net::STATUS_OK)?;
    let params = QueryParams {
        regexp: req.regexp,
        ignore_case: req.ignore_case,
        basename: req.basename,
        existing: req.existing,
        limit: req.limit as usize,
    };
    let result = execute(index, &req.patterns, &params, |path| {
        net::write_result(&mut writer, path)
    });
    net::write_end(&mut writer)?;
    writer.flush()?;
    result.map(|_| ())
}

/// Does the path (raw bytes) still exist on disk? Uses `lstat` so a dangling
/// symlink still counts as present.
fn exists(path: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::fs::symlink_metadata(std::ffi::OsStr::from_bytes(path)).is_ok()
}
