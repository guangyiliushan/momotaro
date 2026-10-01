//! Thin command-line entry point for Momotaro.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use momotaro_run::{
    AskOptions, ProviderTransport, RunError, SearchOutput, WatchLine, ask_workspace,
    coverage_report, doctor, format_ask, format_coverage, format_error, format_health,
    format_index_error, format_index_summary, format_rebuild, format_reconcile,
    format_search_excerpt, format_search_hit, format_stats, format_trace, format_watch_line,
    index_workspace, init_workspace, is_healthy, is_uninitialized, rebuild_index, reconcile,
    search_workspace, stats, trace_workspace, validate_ask_policy, watch,
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

    /// Forbid every LLM call (`ask` refuses; `search` stays lexical).
    #[arg(long, global = true)]
    no_llm: bool,

    /// Allow tools to touch the network (Beta 保守默认: off).
    #[arg(long, global = true)]
    allow_network: bool,

    /// Allow tools to write files (Beta 保守默认: off).
    #[arg(long, global = true)]
    allow_write: bool,

    /// Override the context budget (tokens).
    #[arg(long, global = true)]
    max_context_tokens: Option<u32>,

    /// Retrieval mode: `lexical` (default, zero LLM) or `expand` (one short
    /// LLM rewrite before retrieval).
    #[arg(long, global = true, default_value = "lexical")]
    mode: SearchMode,

    #[command(subcommand)]
    command: Command,
}

/// The `--mode` vocabulary (`docs/docs/usage.md`, "Search").
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SearchMode {
    /// Pure Tantivy BM25, zero LLM tokens.
    Lexical,
    /// One short LLM rewrite, then retrieval over the expanded queries.
    Expand,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize the local SQLite store.
    Init,

    /// Check workspace health without mutating it.
    /// Check the workspace, or read the last ingest batch back
    /// (`--coverage`, ADR 0026).
    Doctor {
        /// Report what the most recent ingest run (`index` or `watch`) did to
        /// each file, read from
        /// the audit log, instead of checking workspace health.
        #[arg(long)]
        coverage: bool,
    },

    /// Print current local store stats.
    Stats,

    /// Index Markdown notes into the local store and search index.
    Index {
        /// Optional vault path override (defaults to `[vault].path` in
        /// `momotaro.toml`).
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

    /// Answer a question from the indexed notes, with citations.
    Ask {
        /// The question (joined with spaces, so quotes are not required).
        query: Vec<String>,
    },

    /// Replay one run: what was retrieved, what the model saw, and how every
    /// citation verified.
    Trace {
        /// The run id `ask` reported.
        run_id: String,
    },

    /// Rebuild the Tantivy index from canonical chunks (canonical data untouched).
    RebuildIndex,

    /// Keep the store current with the vault while it changes (D40).
    Watch,

    /// Bring the store up to date with the whole vault in one pass (D40 §4.4).
    Reconcile,
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
        // The serializer escapes the JSON form; a terminal needs the same care.
        eprintln!("error: {}", format_error(&error));
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
        Command::Doctor { coverage } => {
            if coverage {
                // The audit log's read path (ADR 0026): what the last `index`
                // run did to each file, including the ones it refused.
                return match coverage_report(&root) {
                    Ok(Some(report)) => print_success(
                        cli.json,
                        serde_json::to_string_pretty(&report).expect("coverage report serializes"),
                        &format_coverage(&report),
                    ),
                    Ok(None) => match doctor(&root) {
                        // A workspace that exists but has nothing in it yet: the
                        // advice must be runnable, so `init` and `index` are two
                        // different sentences.
                        Ok(report) if is_uninitialized(&report) => {
                            if cli.json {
                                println!("null");
                            }
                            eprintln!(
                                "no ingest batch recorded yet; this workspace is not initialized, \
                                 run `momotaro init` first"
                            );
                            ExitCode::SUCCESS
                        }
                        Ok(report) if is_healthy(&report) => {
                            if cli.json {
                                println!("null");
                            }
                            eprintln!("no ingest batch recorded yet; run `momotaro index` first");
                            ExitCode::SUCCESS
                        }
                        // Any other state — invalid, foreign, newer schema, or a
                        // directory that is not a workspace at all — has its own
                        // sentence and its own exit code, and they are the ones
                        // `doctor` gives it. Inventing advice here would send the
                        // reader to a command that cannot run.
                        Ok(report) => {
                            let payload = serde_json::to_string_pretty(&report)
                                .expect("health report serializes");
                            print_success(cli.json, payload, &format_health(&report));
                            ExitCode::FAILURE
                        }
                        Err(error) => print_error(cli.json, error),
                    },
                    Err(error) => print_error(cli.json, error),
                };
            }
            match doctor(&root) {
                Ok(report) => {
                    let payload =
                        serde_json::to_string_pretty(&report).expect("health report serializes");
                    print_success(cli.json, payload, &format_health(&report));
                    // A health check that cannot fail a script is not a check
                    // (docs/docs/usage.md, "Exit codes").
                    if is_healthy(&report) {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Stats => match stats(&root) {
            Ok(report) => {
                let payload =
                    serde_json::to_string_pretty(&report).expect("stats report serializes");
                let human = format_stats(&report);
                print_success(cli.json, payload, &human)
            }
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
                        println!("{}", format_index_summary(&report));
                        for item in &report.errors {
                            eprintln!("error: {}", format_index_error(item));
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Search { query, top } => {
            if cli.mode == SearchMode::Expand {
                // `--mode expand` is one short LLM rewrite over the query
                // (usage.md): it needs the provider transport this build does
                // not wire — the same seam `ask` names, refused by name.
                return print_error(
                    cli.json,
                    RunError::ProviderUnavailable(
                        "search --mode expand needs the LLM rewrite step",
                    ),
                );
            }
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
        Command::Ask { query } => {
            let query = query.join(" ");
            let options = AskOptions {
                client_request_id: None,
                no_llm: cli.no_llm,
                allow_network: cli.allow_network,
                allow_write: cli.allow_write,
                max_context_tokens: cli.max_context_tokens,
            };
            if cli.mode == SearchMode::Expand {
                // `--mode` is search's retrieval mode (usage.md): an ask
                // that silently ignored it would be an ask that lied about
                // what it did.
                return print_error(
                    cli.json,
                    RunError::PolicyRefused(
                        "--mode is search's retrieval mode; ask answers with one LLM call regardless",
                    ),
                );
            }
            if let Err(error) = validate_ask_policy(&options) {
                return print_error(cli.json, error);
            }
            let transport = match provider_transport() {
                Ok(transport) => transport,
                Err(error) => return print_error(cli.json, error),
            };
            match ask_workspace(&root, &query, &options, &*transport) {
                Ok(report) => {
                    // A failed run still prints its answer and its explicit
                    // failure marks (architecture §14.1.5) — and still exits
                    // non-zero: nothing is dressed as success (usage.md,
                    // "Exit codes").
                    let exit = if report.status == "finished" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    };
                    if cli.json {
                        let payload =
                            serde_json::to_string_pretty(&report).expect("report serializes");
                        println!("{payload}");
                    } else {
                        print!("{}", format_ask(&report));
                    }
                    exit
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Trace { run_id } => {
            // The run engine's open sweeps (harness §2), so a crashed run
            // reads back as `failed(interrupted)` here — trace 可用.
            match trace_workspace(&root, &run_id) {
                Ok(trace) => {
                    if cli.json {
                        let payload =
                            serde_json::to_string_pretty(&trace).expect("trace serializes");
                        print_success(cli.json, payload, &format_trace(&trace))
                    } else {
                        println!("{}", format_trace(&trace));
                        ExitCode::SUCCESS
                    }
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Watch => {
            // Every line the watcher produces goes through one callback, so
            // `--json` and the human exit cannot promise different fields.
            let json = cli.json;
            let mut emit = |line: &WatchLine| {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(line).expect("watch line serializes")
                    );
                } else {
                    println!("{}", format_watch_line(line));
                }
            };
            match watch(&root, &mut emit) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => print_error(json, error),
            }
        }
        Command::Reconcile => match reconcile(&root) {
            Ok(report) => print_success(
                cli.json,
                serde_json::to_string_pretty(&report).expect("reconcile report serializes"),
                &format_reconcile(&report),
            ),
            Err(error) => print_error(cli.json, error),
        },
        Command::RebuildIndex => match rebuild_index(&root) {
            Ok(report) => {
                if cli.json {
                    print_success(
                        cli.json,
                        serde_json::to_string_pretty(&report).expect("rebuild report serializes"),
                        "",
                    );
                } else {
                    println!("{}", format_rebuild(&report));
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
        println!("{}", format_search_hit(hit));
        println!("    {}", format_search_excerpt(&hit.excerpt));
    }
}

/// The provider seam (the CLI's only LLM wiring point).
///
/// `momotaro-llm` deliberately holds no HTTP client — every exchange goes
/// through this trait ("production wires a real transport later"), and this
/// build ships no production transport: wiring one (an OpenAI-compatible
/// client behind `--allow-network`) is a registered dependency decision.
/// Until it lands, `ask` and `search --mode expand` refuse by name here —
/// before any run row exists — while the engine itself is proven end to end
/// by the fake-LLM integration suite (`architecture.md` §23 DoD 9).
fn provider_transport() -> Result<Box<dyn ProviderTransport>, RunError> {
    Err(RunError::ProviderUnavailable("ask answers with an LLM"))
}
