//! `rlocate` — search an rlocate index locally, serve it over the network, or
//! query a remote server.

use anyhow::{bail, Context, Result};
use clap::Parser;
use rlocate_core::index::Index;
use rlocate_core::matcher::{looks_like_glob, MatchKind, Options, Query};
use rlocate_core::{net, search};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
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

    /// Limit output to this many matches (0 = unlimited).
    #[arg(short = 'l', long = "limit", visible_alias = "n", default_value_t = 0)]
    limit: usize,

    /// Separate results with NUL instead of newline.
    #[arg(short = '0', long = "null")]
    null: bool,

    /// Print statistics about the index and exit (local only).
    #[arg(short = 'S', long = "statistics")]
    statistics: bool,

    /// Run as a query server instead of searching.
    #[arg(long = "serve", conflicts_with = "host")]
    serve: bool,

    /// Address to listen on in --serve mode.
    #[arg(long = "listen", value_name = "ADDR", default_value = "127.0.0.1:9999")]
    listen: String,

    /// Query a remote rlocate server at this ADDR (host:port) instead of a
    /// local index.
    #[arg(long = "host", value_name = "ADDR")]
    host: Option<String>,

    /// Shared secret: required by --serve, and sent with each --host query.
    #[arg(long = "token", value_name = "TOKEN")]
    token: Option<String>,
}

/// The matching parameters shared by local, server, and remote execution.
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
                // GNU locate exits 1 when nothing matched.
                ExitCode::from(1)
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
    if args.host.is_some() {
        if args.patterns.is_empty() {
            bail!("no pattern given");
        }
        return query_remote(&args);
    }

    // Local search.
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

impl Args {
    fn query_params(&self) -> QueryParams {
        QueryParams {
            regexp: self.regexp,
            ignore_case: self.ignore_case,
            // -w (wholename) overrides -b (basename).
            basename: self.basename && !self.wholename,
            existing: self.existing,
            limit: self.limit,
        }
    }
}

/// Run the search for every pattern against `index`, invoking `on_match` for
/// each surviving match (after the `existing` filter), honouring `limit`.
/// Returns the number of matches emitted.
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
                return true; // skip missing file, keep searching
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

fn print_statistics<W: Write>(out: &mut W, index: &Index, db_path: &std::path::Path) -> Result<()> {
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

/// Run the query server: load the index once and serve clients concurrently.
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

/// Query a remote rlocate server and print the streamed results.
fn query_remote(args: &Args) -> Result<bool> {
    let host = args.host.as_ref().expect("host set");
    let p = args.query_params();
    let request = net::Request {
        token: args.token.clone().unwrap_or_default(),
        regexp: p.regexp,
        ignore_case: p.ignore_case,
        basename: p.basename,
        existing: p.existing,
        limit: p.limit as u32,
        patterns: args.patterns.clone(),
    };

    let stream = TcpStream::connect(host).with_context(|| format!("connecting to {host}"))?;
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    request.write_to(&mut writer)?;

    match net::read_status(&mut reader)? {
        net::STATUS_OK => {}
        net::STATUS_AUTH_FAILED => bail!("remote: {}", net::read_message(&mut reader)?),
        _ => bail!("remote error: {}", net::read_message(&mut reader)?),
    }

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let sep = if args.null { 0u8 } else { b'\n' };
    let mut total = 0usize;
    while let Some(path) = net::read_result(&mut reader)? {
        total += 1;
        if !args.count {
            out.write_all(&path)?;
            out.write_all(&[sep])?;
        }
    }
    if args.count {
        writeln!(out, "{total}")?;
    }
    out.flush()?;
    Ok(total > 0)
}

/// Does the path (raw bytes) still exist on disk? Uses `lstat` so a dangling
/// symlink still counts as present (the directory entry exists).
fn exists(path: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let os = std::ffi::OsStr::from_bytes(path);
    std::fs::symlink_metadata(os).is_ok()
}
