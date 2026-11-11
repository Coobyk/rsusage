mod agents;
mod app;
mod cli;
mod live;
mod paths;
mod pricing;
mod render;
mod report;
mod statusline;
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
    let cli = match Cli::from_arg_matches(&cmd.get_matches()) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    if let Err(e) = dispatch(&cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    match &cli.cmd {
        None => app::run_report(Report::Daily, &cli.default, None),
        Some(Cmd::Daily(a)) => app::run_report(Report::Daily, a, None),
        Some(Cmd::Weekly(a)) => app::run_report(Report::Weekly, a, None),
        Some(Cmd::Monthly(a)) => app::run_report(Report::Monthly, a, None),
        Some(Cmd::Session(a)) => app::run_report(Report::Session, a, None),
        Some(Cmd::Blocks(b)) => app::run_blocks(b, None),
        Some(Cmd::Statusline(s)) => app::run_statusline(s, None),
        Some(Cmd::Agent(argv)) => app::run_agent(argv),
    }
}
