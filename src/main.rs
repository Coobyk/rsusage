mod agents;
mod app;
mod cli;
mod cliparse;
mod live;
mod paths;
mod pricing;
mod render;
mod report;
mod statusline;
mod table;
mod types;

use clap::{CommandFactory, FromArgMatches};
use cli::*;
use report::Report;

// Short-lived, allocation-heavy process (bincode decode + JSON parse):
// mimalloc beats the glibc allocator on small-alloc churn and has ~zero
// init cost, unlike jemalloc's per-thread arenas.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // clap `external_subcommand` catches `rsusage <agent> ...`; list the
    // known agents in help so they're still discoverable.
    let cmd = Cli::command().after_help(format!(
        "Agent subcommands: {}\nExample: rsusage opencode daily --json",
        agents::agent_ids().join(", ")
    ));
    // upstream-faithful pre-parser: exits(2) with upstream messages on any
    // parse error, otherwise returns argv canonicalized to `--long` flags
    let argv = cliparse::normalize_args();
    let matches = match cmd.try_get_matches_from(&argv) {
        Ok(m) => m,
        Err(e) => e.exit(),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    if let Err(e) = dispatch(&cli) {
        // upstream `CliError(String)` — Debug-shown as `Error: CliError("…")`
        eprintln!("Error: CliError({:?})", e.to_string());
        std::process::exit(1);
    }
}

fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    // upstream parses shared flags before the command token and hands them
    // to the command's parser as defaults — merge root args into the
    // subcommand's (the subcommand's explicit values win)
    let root = &cli.default;
    match &cli.cmd {
        None => app::run_report(Report::Daily, root, None),
        Some(Cmd::Daily(a)) => app::run_report(Report::Daily, &root.merged(a), None),
        Some(Cmd::Weekly(a)) => app::run_report(Report::Weekly, &root.merged(a), None),
        Some(Cmd::Monthly(a)) => app::run_report(Report::Monthly, &root.merged(a), None),
        Some(Cmd::Session(a)) => app::run_report(Report::Session, &root.merged(a), None),
        Some(Cmd::Blocks(b)) => app::run_blocks(&b.merged(root), None),
        Some(Cmd::Statusline(s)) => app::run_statusline(&s.merged(root), None),
        Some(Cmd::Agent(argv)) => app::run_agent(argv, root),
    }
}
