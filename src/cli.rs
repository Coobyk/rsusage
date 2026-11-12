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
    #[arg(short = 'o', long)]
    pub order: Option<Order>,
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
    /// Project alias mapping (claude daily)
    #[arg(long)]
    pub project_aliases: Option<String>,
    /// jq filter applied to JSON output (piped to the `jq` binary)
    #[arg(short = 'q', long)]
    pub jq: Option<String>,
    /// Codex speed tier (codex only)
    #[arg(long)]
    pub speed: Option<String>,
    /// Custom pi data path (pi only)
    #[arg(long)]
    pub pi_path: Option<String>,
    /// Custom openclaw data path (openclaw only)
    #[arg(long)]
    pub open_claw_path: Option<String>,
    /// Disable parallel JSONL file loading
    #[arg(long)]
    pub single_thread: bool,
}

impl FilterArgs {
    /// `self` = root-level shared flags, `sub` = the command's own flags —
    /// upstream hands accumulated root flags to the command parser as
    /// defaults, so the command's explicit values win.
    fn merged(&self, sub: &FilterArgs) -> FilterArgs {
        FilterArgs {
            json: self.json || sub.json,
            since: sub.since.clone().or_else(|| self.since.clone()),
            until: sub.until.clone().or_else(|| self.until.clone()),
            timezone: sub.timezone.clone().or_else(|| self.timezone.clone()),
            mode: if sub.mode == crate::report::CostMode::default() {
                self.mode
            } else {
                sub.mode
            },
            breakdown: self.breakdown || sub.breakdown,
            order: sub.order.or(self.order),
            offline: self.offline || sub.offline,
            no_offline: self.no_offline || sub.no_offline,
            all: self.all || sub.all,
            by_agent: self.by_agent || sub.by_agent,
            compact: self.compact || sub.compact,
            no_cost: self.no_cost || sub.no_cost,
            color: self.color || sub.color,
            no_color: self.no_color || sub.no_color,
            config: sub.config.clone().or_else(|| self.config.clone()),
            instances: self.instances || sub.instances,
            debug: self.debug || sub.debug,
            debug_samples: sub.debug_samples.or(self.debug_samples),
            project: sub.project.clone().or_else(|| self.project.clone()),
            project_aliases: sub
                .project_aliases
                .clone()
                .or_else(|| self.project_aliases.clone()),
            jq: sub.jq.clone().or_else(|| self.jq.clone()),
            speed: sub.speed.clone().or_else(|| self.speed.clone()),
            pi_path: sub.pi_path.clone().or_else(|| self.pi_path.clone()),
            open_claw_path: sub
                .open_claw_path
                .clone()
                .or_else(|| self.open_claw_path.clone()),
            single_thread: self.single_thread || sub.single_thread,
        }
    }
}

impl ReportArgs {
    /// Merge root-level shared flags (`self`) under the command's own (`sub`).
    pub fn merged(&self, sub: &ReportArgs) -> ReportArgs {
        ReportArgs {
            filter: self.filter.merged(&sub.filter),
            sections: sub.sections.clone().or_else(|| self.sections.clone()),
            last: sub.last.or(self.last),
            start_of_week: sub.start_of_week.or(self.start_of_week),
            id: sub.id.clone().or_else(|| self.id.clone()),
        }
    }
}

impl BlocksArgs {
    /// Merge root-level shared flags under the blocks command's own.
    pub fn merged(&self, root: &ReportArgs) -> BlocksArgs {
        let mut out = self.clone();
        out.filter = root.filter.merged(&self.filter);
        out
    }
}

impl StatusArgs {
    /// Merge root-level shared flags under statusline's own — upstream seeds
    /// statusline's timezone/config from the accumulated root shared args.
    pub fn merged(&self, root: &ReportArgs) -> StatusArgs {
        let mut out = self.clone();
        if out.timezone.is_none() {
            out.timezone = root.filter.timezone.clone();
        }
        if out.config.is_none() {
            out.config = root.filter.config.clone();
        }
        out
    }
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
    /// Show a single session's detail report (session only)
    #[arg(long)]
    pub id: Option<String>,
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
    #[arg(short = 'n', long, default_value_t = DEFAULT_SESSION_HOURS as f64)]
    pub session_length: f64,
}

#[derive(Args, Clone)]
pub struct StatusArgs {
    /// Use cached pricing data instead of fetching (default on for statusline)
    #[arg(short = 'O', long, default_value_t = true)]
    pub offline: bool,
    /// Allow fetching fresh pricing data
    #[arg(long)]
    pub no_offline: bool,
    /// Burn-rate indicator: off|emoji|text|emoji-text
    #[arg(short = 'B', long, default_value = "off")]
    pub visual_burn_rate: String,
    /// Session cost source: auto|ccusage|cc|both
    #[arg(long, default_value = "auto")]
    pub cost_source: String,
    /// Enable the per-session output cache (default on)
    #[arg(long, default_value_t = true)]
    pub cache: bool,
    /// Disable the per-session output cache
    #[arg(long)]
    pub no_cache: bool,
    /// Seconds a cached line stays fresh
    #[arg(long, default_value_t = 1)]
    pub refresh_interval: u64,
    /// Context-remaining green threshold (percent)
    #[arg(long, default_value_t = 50)]
    pub context_low_threshold: u8,
    /// Context-remaining yellow threshold (percent)
    #[arg(long, default_value_t = 80)]
    pub context_medium_threshold: u8,
    /// Timezone for date grouping (IANA)
    #[arg(short = 'z', long)]
    pub timezone: Option<String>,
    /// Path to a config file (accepted for flag compatibility)
    #[arg(long)]
    pub config: Option<String>,
    /// Enable debug output
    #[arg(long)]
    pub debug: bool,
}

impl Default for StatusArgs {
    fn default() -> Self {
        Self {
            offline: true,
            no_offline: false,
            visual_burn_rate: "off".into(),
            cost_source: "auto".into(),
            cache: true,
            no_cache: false,
            refresh_interval: 1,
            context_low_threshold: 50,
            context_medium_threshold: 80,
            timezone: None,
            config: None,
            debug: false,
        }
    }
}
