use clap::{Args, Parser, Subcommand, ValueEnum};

/// Default billing-block length (hours); shared by `blocks -n` and statusline.
pub const DEFAULT_SESSION_HOURS: i64 = 5;

#[derive(Parser)]
#[command(
    name = "rsusage",
    version,
    about = "Blazing fast usage reports for coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    #[command(flatten)]
    pub default: ReportArgs,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Show all detected coding agent usage grouped by date
    Daily(ReportArgs),
    /// Show all detected coding agent usage grouped by month
    Monthly(ReportArgs),
    /// Show all detected coding agent usage grouped by week
    Weekly(ReportArgs),
    /// Show all detected coding agent usage grouped by session
    Session(ReportArgs),
    /// Show usage report grouped by session billing blocks
    Blocks(BlocksArgs),
    /// Display compact status line for Claude Code hooks (Beta)
    Statusline(StatusArgs),
    /// Per-agent reports: `rsusage <agent> <daily|weekly|monthly|session|blocks|statusline>`
    #[command(external_subcommand)]
    Agent(Vec<String>),
}

/// Reparse of an agent-scoped invocation (`rsusage opencode daily --json`
/// arrives as `Agent(["opencode", "daily", "--json"])`).
#[derive(Parser)]
pub struct AgentCli {
    #[command(subcommand)]
    pub cmd: Option<AgentCmd>,
    /// bare `rsusage <agent> [flags]` defaults to a daily report
    #[command(flatten)]
    pub default: ReportArgs,
}

#[derive(Subcommand)]
pub enum AgentCmd {
    Daily(ReportArgs),
    Monthly(ReportArgs),
    Weekly(ReportArgs),
    Session(ReportArgs),
    Blocks(BlocksArgs),
    Statusline(StatusArgs),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Weekday {
    Sunday,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

impl Weekday {
    /// Weekday index (Sunday=0 .. Saturday=6) used by week bucketing.
    pub fn index(self) -> i64 {
        match self {
            Self::Sunday => 0,
            Self::Monday => 1,
            Self::Tuesday => 2,
            Self::Wednesday => 3,
            Self::Thursday => 4,
            Self::Friday => 5,
            Self::Saturday => 6,
        }
    }
}

/// Flags every report-like command shares.
#[derive(Args, Clone, Debug, Default)]
pub struct FilterArgs {
    /// Output in JSON format
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Filter from date (YYYY-MM-DD or YYYYMMDD)
    #[arg(short = 's', long)]
    pub since: Option<String>,
    /// Filter until date (inclusive)
    #[arg(short = 'u', long)]
    pub until: Option<String>,
    /// Timezone for date grouping (IANA)
    #[arg(short = 'z', long)]
    pub timezone: Option<String>,
    /// Cost calculation mode
    #[arg(short = 'm', long, default_value = "auto")]
    pub mode: crate::report::CostMode,
    /// Show per-model cost breakdown
    #[arg(short = 'b', long)]
    pub breakdown: bool,
    /// Sort order
    #[arg(short = 'o', long, default_value = "asc")]
    pub order: Order,
    /// Use cached pricing data instead of fetching
    #[arg(short = 'O', long, overrides_with = "no_offline")]
    pub offline: bool,
    /// Negatable of --offline
    #[arg(long, overrides_with = "offline")]
    pub no_offline: bool,
    /// Accepted for compatibility; all detected agents included by default
    #[arg(long)]
    pub all: bool,
    /// Include per-agent JSON breakdowns in unified report rows
    #[arg(long)]
    pub by_agent: bool,
    /// Force compact table layout
    #[arg(long)]
    pub compact: bool,
    /// Hide cost information
    #[arg(long)]
    pub no_cost: bool,
    /// Enable colored output (FORCE_COLOR=1 has same effect)
    #[arg(long, overrides_with = "no_color")]
    pub color: bool,
    /// Disable colored output (NO_COLOR=1 has same effect)
    #[arg(long, overrides_with = "color")]
    pub no_color: bool,
    /// Path to configuration file
    #[arg(long)]
    pub config: Option<String>,
    /// Aggregate usage across all configured Claude instances
    #[arg(long)]
    pub instances: bool,
    /// Show pricing mismatch information for debugging
    #[arg(short = 'd', long)]
    pub debug: bool,
    /// Number of sample discrepancies to show in debug output
    #[arg(long)]
    pub debug_samples: Option<u32>,
    /// Filter to a specific project
    #[arg(short = 'p', long)]
    pub project: Option<String>,
    /// Disable parallel JSONL file loading
    #[arg(long)]
    pub single_thread: bool,
}

/// Period-report flags (daily/weekly/monthly/session).
#[derive(Args, Clone, Debug, Default)]
pub struct ReportArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Emit multiple report sections from one load (daily,weekly,monthly,session)
    #[arg(long)]
    pub sections: Option<String>,
    /// Show only the most recent N periods
    #[arg(long)]
    pub last: Option<u32>,
    /// Start day of the week
    #[arg(short = 'w', long)]
    pub start_of_week: Option<Weekday>,
}

#[derive(Args, Clone, Debug, Default)]
pub struct BlocksArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Live-updating report view
    #[arg(long)]
    pub live: bool,
    /// Show only active block with projections
    #[arg(short = 'a', long)]
    pub active: bool,
    /// Show blocks from last 3 days (including active)
    #[arg(short = 'r', long)]
    pub recent: bool,
    /// Token limit for quota warnings (e.g., 500000 or "max")
    #[arg(short = 't', long)]
    pub token_limit: Option<String>,
    /// Session block duration in hours
    #[arg(short = 'n', long, default_value_t = DEFAULT_SESSION_HOURS)]
    pub session_length: i64,
}

#[derive(Args, Clone, Default)]
pub struct StatusArgs {
    /// Output in JSON format
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Use cached pricing data instead of fetching
    #[arg(short = 'O', long)]
    pub offline: bool,
    /// Timezone for date grouping (IANA)
    #[arg(short = 'z', long)]
    pub timezone: Option<String>,
}
