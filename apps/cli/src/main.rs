//! Thin command-line entry point for Momotaro.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use momotaro_run::{
    RunError, SearchOutput, doctor, index_workspace, init_workspace, rebuild_index,
    search_workspace, stats,
};

/// A personal knowledge engine for serious learners.
#[derive(Debug, Parser)]
#[command(name = "momotaro", version, about)]
struct Cli {
    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Reserved for future timing output.
    #[arg(long, global = true)]
    profile: bool,

    /// Run without an LLM backend. Explicit no-op in v0.1.
    #[arg(long, global = true)]
    no_llm: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize the local SQLite store.
    Init,

    /// Check workspace health without mutating it.
    Doctor,

    /// Print current local store stats.
    Stats,

    /// Index Markdown notes into the local store and search index.
    Index {
        /// Optional vault path override (defaults to momotaro.toml [vault].path).
        path: Option<PathBuf>,
    },

    /// Search indexed notes with Tantivy BM25 (zero LLM).
    Search {
        /// Search terms (joined with spaces, so quotes are not required).
        query: Vec<String>,

        /// Maximum number of hits.
        #[arg(long, short = 'n', default_value_t = 10)]
        top: usize,
    },

    /// Rebuild the Tantivy index from canonical chunks (canonical data untouched).
    RebuildIndex,
}

fn print_success(json: bool, payload: String, human: &str) -> ExitCode {
    if json {
        println!("{payload}");
    } else {
        println!("{human}");
    }

    ExitCode::SUCCESS
}

fn print_error(json: bool, error: RunError) -> ExitCode {
    if json {
        println!("{}", serde_json::json!({ "error": error.to_string() }));
    } else {
        eprintln!("error: {error}");
    }

    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            // The shell's cwd can disappear (e.g. removed while the terminal
            // sat in it); that is a user-facing error, not a panic.
            if cli.json {
                println!("{}", serde_json::json!({ "error": error.to_string() }));
            } else {
                eprintln!("error: cannot determine working directory: {error}");
            }
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        Command::Init => match init_workspace(&root) {
            Ok(report) => print_success(
                cli.json,
                serde_json::to_string_pretty(&report).expect("stats report serializes"),
                "workspace initialized",
            ),
            Err(error) => print_error(cli.json, error),
        },
        Command::Doctor => match doctor(&root) {
            Ok(report) => print_success(
                cli.json,
                serde_json::to_string_pretty(&report).expect("health report serializes"),
                "workspace is ready",
            ),
            Err(error) => print_error(cli.json, error),
        },
        Command::Stats => match stats(&root) {
            Ok(report) => print_success(
                cli.json,
                serde_json::to_string_pretty(&report).expect("stats report serializes"),
                "stats complete",
            ),
            Err(error) => print_error(cli.json, error),
        },
        Command::Index { path } => {
            let path_override = path.as_deref();
            match index_workspace(&root, path_override) {
                Ok(report) => {
                    if cli.json {
                        print_success(
                            cli.json,
                            serde_json::to_string_pretty(&report).expect("index report serializes"),
                            "",
                        );
                    } else {
                        println!(
                            "indexed {} of {} files ({} new revisions, {} unchanged), {} chunks, {} errors in {} ms",
                            report.files_indexed,
                            report.files_scanned,
                            report.revisions_created,
                            report.files_skipped,
                            report.chunks_written,
                            report.errors.len(),
                            report.duration_ms
                        );
                        for item in &report.errors {
                            eprintln!("error: {}: {}", item.source_key, item.message);
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Search { query, top } => {
            let query = query.join(" ");
            match search_workspace(&root, &query, top) {
                Ok(output) => {
                    if cli.json {
                        let payload = serde_json::to_string_pretty(&output)
                            .expect("search output serializes");
                        print_success(cli.json, payload, "")
                    } else {
                        print_hits(&output);
                        ExitCode::SUCCESS
                    }
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::RebuildIndex => match rebuild_index(&root) {
            Ok(report) => {
                if cli.json {
                    print_success(
                        cli.json,
                        serde_json::to_string_pretty(&report).expect("rebuild report serializes"),
                        "",
                    );
                } else {
                    println!(
                        "rebuilt index over {} chunks in {} ms",
                        report.chunks, report.duration_ms
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => print_error(cli.json, error),
        },
    }
}

fn print_hits(output: &SearchOutput) {
    if output.hits.is_empty() {
        println!("no results");
        return;
    }

    for hit in &output.hits {
        let title = hit.title.as_deref().unwrap_or("-");
        println!(
            "{}. {}#{}  {}  score={:.3}",
            hit.hit.rank, hit.hit.source_key, hit.hit.ordinal, title, hit.hit.score
        );
        let excerpt = hit.excerpt.replace('\n', " ");
        if excerpt.chars().count() > 100 {
            let truncated: String = excerpt.chars().take(100).collect();
            println!("    {truncated}...");
        } else {
            println!("    {excerpt}");
        }
    }
}
