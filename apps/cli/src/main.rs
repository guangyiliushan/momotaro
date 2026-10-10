//! Thin command-line entry point for Momotaro.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use momotaro_run::{
    AskOptions, RunError, SearchOutput, WatchLine, ask_workspace, coverage_report, doctor,
    expand_search_workspace, format_ask, format_coverage, format_error, format_health,
    format_index_error, format_index_summary, format_rebuild, format_reconcile,
    format_search_excerpt, format_search_hit, format_stats, format_trace, format_watch_line,
    index_workspace, init_workspace, is_healthy, is_uninitialized, provider_transport,
    rebuild_index, reconcile, search_workspace, stats, trace_workspace, validate_ask_policy,
    validate_expand_policy, validate_network_policy, watch,
};

/// A personal knowledge engine for serious learners.
#[derive(Debug, Parser)]
#[command(name = "momotaro", version, about)]
struct Cli {
    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Forbid every LLM call (`ask` and `--mode expand` refuse; lexical
    /// `search` is unaffected).
    #[arg(long, global = true)]
    no_llm: bool,

    /// Allow network access (Beta 保守默认: off): `ask` and
    /// `--mode expand` are refused without it.
    #[arg(long, global = true)]
    allow_network: bool,

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

    /// Check workspace health without mutating it, or read the last ingest
    /// batch back (`--coverage`, ADR 0026).
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

    /// Search indexed notes with Tantivy BM25 (`--mode expand` spends one LLM
    /// rewrite before retrieving).
    Search {
        /// Search terms (joined with spaces, so quotes are not required).
        query: Vec<String>,

        /// Maximum number of hits (0 is legal and returns nothing).
        #[arg(long, short = 'n', default_value_t = 10)]
        top: usize,

        /// Retrieval mode: `lexical` (default, zero LLM) or `expand` (one
        /// short LLM rewrite before retrieval).
        #[arg(long, default_value = "lexical")]
        mode: SearchMode,
    },

    /// Answer a question from the indexed notes, with citations.
    Ask {
        /// The question (joined with spaces, so quotes are not required).
        query: Vec<String>,

        /// Continue an existing session instead of opening a new one.
        ///
        /// The value is required and must name a session that exists; a typo
        /// is an error rather than a silently new conversation. Without the
        /// flag `ask` opens a fresh session every time.
        #[arg(long)]
        session: Option<String>,

        /// Make one logical request replay-safe across CLI retries.
        #[arg(long)]
        client_request_id: Option<String>,

        /// Retrieval mode. `ask` answers with one LLM call regardless — the
        /// flag exists to say so when it is given `expand` (usage.md), not to
        /// change what ask does.
        #[arg(long, default_value = "lexical")]
        mode: SearchMode,

        /// Allow tools to write files (Beta 保守默认: off).
        #[arg(long)]
        allow_write: bool,

        /// Override the context budget (tokens).
        #[arg(long)]
        max_context_tokens: Option<u32>,
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
        // The same safe string as the human outlet — `format_error` is the
        // sanitizer on both sides: a bidi override in a path must not ride
        // the machine-readable channel either.
        println!("{}", serde_json::json!({ "error": format_error(&error) }));
    } else {
        // The serializer escapes the JSON form; a terminal needs the same care.
        eprintln!("error: {}", format_error(&error));
    }

    ExitCode::FAILURE
}

/// The cap on `--top` (search): the pre-allocation tantivy does is driven by
/// this number alone (probe S1-2: 20M -> 460 MB peak commit), so a typo must
/// not become a memory spike. `--top 0` stays legal — it returns nothing.
const SEARCH_TOP_CAP: usize = 1000;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            // The shell's cwd can disappear (e.g. removed while the terminal
            // sat in it); that is a user-facing error, not a panic. (The one
            // error outlet that sanitizes nothing: the text is the OS's about
            // a directory the process cannot name and carries no
            // attacker-authored bytes — registered, S5/A4 + adversarial #3.)
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
                serde_json::to_string_pretty(&report).expect("init report serializes"),
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
        Command::Search { query, top, mode } => {
            if top > SEARCH_TOP_CAP {
                // Both outlets, like every other refusal: `--json` gets the
                // error object, the terminal the sentence (adversarial lane
                // #2: the first cut wrote stderr only and left stdout empty
                // under `--json`).
                let message = format!(
                    "--top {top} is above the cap of {SEARCH_TOP_CAP} \
                     (the pre-allocation is driven by this number alone)"
                );
                if cli.json {
                    println!("{}", serde_json::json!({ "error": message }));
                }
                eprintln!("error: {message}");
                return ExitCode::FAILURE;
            }
            let query = query.join(" ");
            let output = match mode {
                SearchMode::Lexical => search_workspace(&root, &query, top),
                SearchMode::Expand => {
                    // Policy before endpoint: `--no-llm` and the network gate
                    // are refusals about the request itself, and must not be
                    // reported as whatever this workspace happens to be missing.
                    if let Err(error) = validate_expand_policy(cli.no_llm) {
                        return print_error(cli.json, error);
                    }
                    if let Err(error) = validate_network_policy(&root, cli.allow_network) {
                        return print_error(cli.json, error);
                    }
                    // Built here and passed in, so tests drive the fake rig and
                    // this process never dials on its own.
                    let transport = match provider_transport(&root) {
                        Ok(transport) => transport,
                        Err(error) => return print_error(cli.json, error),
                    };
                    expand_search_workspace(
                        &root,
                        &query,
                        top,
                        cli.no_llm,
                        cli.allow_network,
                        &*transport,
                    )
                }
            };
            match output {
                Ok(output) => {
                    if cli.json {
                        let payload = serde_json::to_string_pretty(&output)
                            .expect("search output serializes");
                        print_success(cli.json, payload, "")
                    } else {
                        for line in search_output_lines(&output) {
                            println!("{line}");
                        }
                        ExitCode::SUCCESS
                    }
                }
                Err(error) => print_error(cli.json, error),
            }
        }
        Command::Ask {
            query,
            session,
            client_request_id,
            mode,
            allow_write,
            max_context_tokens,
        } => {
            let query = query.join(" ");
            let options = AskOptions {
                client_request_id,
                no_llm: cli.no_llm,
                allow_network: cli.allow_network,
                allow_write,
                max_context_tokens,
                session_id: session,
            };
            if mode == SearchMode::Expand {
                // `--mode` is search's retrieval mode (usage.md): an ask
                // that silently ignored it would be an ask that lied about
                // what it did.
                return print_error(
                    cli.json,
                    RunError::PolicyRefused(
                        "--mode is search's retrieval mode; ask answers with one LLM call regardless"
                            .to_owned(),
                    ),
                );
            }
            if let Err(error) = validate_ask_policy(&options) {
                return print_error(cli.json, error);
            }
            // Same two gates expand uses, in the same order: policy first, so a
            // refused request is never reported as a missing endpoint.
            if let Err(error) = validate_network_policy(&root, cli.allow_network) {
                return print_error(cli.json, error);
            }
            // The transport is built from `[model]` + the environment
            // (`momotaro_run::provider_transport`), and a workspace with no
            // endpoint configured is refused here — before any run row exists.
            let transport = match provider_transport(&root) {
                Ok(transport) => transport,
                Err(error) => return print_error(cli.json, error),
            };
            match ask_workspace(&root, &query, &options, &*transport) {
                Ok(report) => {
                    // A failed run still prints its answer and its explicit
                    // failure marks (architecture §14.1.5) — and still exits
                    // non-zero: nothing is dressed as success (usage.md,
                    // "Exit codes").
                    let exit = if ask_succeeded(&report.status) {
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

/// What a human sees for one search, in order.
///
/// First the run `--mode expand` created — there is no "list runs", so for a
/// search run this line is the only way to reach it — then the hits, or
/// `no results`.
///
/// Returned rather than printed so a test can drive it: the hits half through
/// the binary on every platform, the run line through
/// `a_search_that_created_a_run_names_it_in_the_human_outlet`, and the hop to
/// the terminal through `the_outlet_prints_the_renderer_whole`.
fn search_output_lines(output: &SearchOutput) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(run_id) = &output.run_id {
        lines.push(format!("run {run_id}"));
    }
    if output.hits.is_empty() {
        lines.push("no results".to_owned());
        return lines;
    }

    for hit in &output.hits {
        lines.push(format_search_hit(hit));
        lines.push(format!("    {}", format_search_excerpt(&hit.excerpt)));
    }
    lines
}

/// Whether one completed `ask` succeeded (`usage.md`, "Exit codes"): a run
/// that reached `finished` does, everything else does not. The caller turns
/// it into `ExitCode`.
///
/// Named rather than left inline because driving the binary to a *completed*
/// run needs a live endpoint, which the zero-network rule keeps out of CI —
/// so every `ask` test in `apps/cli/tests` stops at a policy refusal and this
/// branch has no end-to-end witness. It returns `bool` rather than `ExitCode`
/// because `ExitCode` is opaque and has nothing to assert against.
fn ask_succeeded(status: &str) -> bool {
    status == "finished"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions, because both promises are load-bearing: a `finished`
    /// run that exited non-zero would fail every script, and a `failed` run
    /// that exited zero would dress a failure as success — the exact claim
    /// `usage.md` "Exit codes" makes.
    #[test]
    fn the_ask_exit_verdict_follows_the_run_status() {
        assert!(ask_succeeded("finished"), "a run that finished is success");
        for status in ["failed", "cancelled", "accepted", "running"] {
            assert!(
                !ask_succeeded(status),
                "`{status}` must not exit 0 — nothing is dressed as success"
            );
        }
    }

    #[test]
    fn ask_parses_an_idempotency_key() {
        let parsed = Cli::try_parse_from([
            "momotaro",
            "ask",
            "--client-request-id",
            "request-1",
            "谱定理",
        ])
        .expect("the idempotency key parses");
        let Command::Ask {
            query,
            client_request_id,
            ..
        } = parsed.command
        else {
            panic!("ask was parsed");
        };
        assert_eq!(client_request_id.as_deref(), Some("request-1"));
        assert_eq!(query.join(" "), "谱定理");
    }
    #[test]
    fn a_search_that_created_a_run_names_it_in_the_human_outlet() {
        let output = SearchOutput {
            query: "谱定理".to_owned(),
            hits: Vec::new(),
            run_id: Some("run-1-2".to_owned()),
        };
        assert_eq!(
            search_output_lines(&output),
            vec!["run run-1-2".to_owned(), "no results".to_owned()],
            "the run line comes first, so a reader sees it before the hits"
        );
    }

    /// The outlet prints every line the renderer returns, whole.
    ///
    /// Structural, on the same precedent as this repo's `include_str!` pins:
    /// nothing in-process can capture `println!`, and an end-to-end witness
    /// for `--mode expand` would need a real provider endpoint, which the
    /// zero-network rule keeps out. The renderer itself is pinned by
    /// `a_search_that_created_a_run_names_it_in_the_human_outlet` and
    /// `a_lexical_search_names_no_run_it_never_created`, and the hits half by
    /// the binary-level test in `apps/cli/tests/cli.rs`; this pins the single
    /// hop between renderer and terminal — deleting the `run` line at the call
    /// site, filtering it out of the loop, or routing around the renderer
    /// entirely is what it exists to catch. What stays registered is an
    /// *end-to-end* witness for `--mode expand` itself.
    #[test]
    fn the_outlet_prints_the_renderer_whole() {
        // Needles are `concat!`-built so this test's own source cannot satisfy
        // them (the counting rule).
        let source = include_str!("main.rs");
        // Anchor at the start of the **human branch**, not at the `for`: a
        // slice that began at the loop could never see a statement written
        // above it, which is precisely the hole the previous anchor had.
        let route = concat!(
            "print_success(cli.json, payload, \"\")\n",
            "                    } else {"
        );
        let at = source
            .find(route)
            .expect("the human outlet routes through the renderer");
        let tail = &source[at..];
        let stop = tail
            .find(concat!("ExitCode::", "SUCCESS"))
            .expect("the arm ends there");
        let block = &tail[..stop];
        // Past the header: whatever sits between `} else {` and the exit code.
        // Dropping the header's text loses `route` above and never gets here;
        // changing what follows changes this slice.
        let after_header = &block[route.len()..];
        let lines: Vec<&str> = after_header
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            lines,
            vec![
                concat!("for line in search", "_output_lines(&output) {"),
                concat!("println!", "(\"{line}\");"),
                "}",
            ],
            "the human branch is the loop and its print and nothing else — no \
             statement above the loop, no filter, no guard, no second path: {block}"
        );
    }

    #[test]
    fn a_lexical_search_names_no_run_it_never_created() {
        let output = SearchOutput {
            query: "谱定理".to_owned(),
            hits: Vec::new(),
            run_id: None,
        };
        assert_eq!(
            search_output_lines(&output),
            vec!["no results".to_owned()],
            "lexical writes no run, so it must not print a line pointing at one"
        );
    }
}
