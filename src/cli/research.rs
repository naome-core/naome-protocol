use crate::wire::Id;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct CaptureArguments {
    pub(super) file: PathBuf,
    pub(super) media: String,
    #[arg(long, default_value_t = 1048576)]
    pub(super) chunk_size: usize,
    #[arg(long)]
    pub(super) access: Option<Id>,
    /// Link the captured evidence to a local question, plan, occurrence or derivation.
    #[arg(long = "for", value_name = "RECORD")]
    pub(super) context: Option<Id>,
    /// Record an explicit note about this use of the evidence.
    #[arg(long, requires = "context")]
    pub(super) note: Option<String>,
}

#[derive(Args)]
pub struct QuestionArguments {
    pub(super) id: Id,
    /// Use this snapshot instead of the active search snapshot.
    #[arg(long)]
    pub(super) head: Option<Id>,
    /// Maximum record and relationship candidates to inspect, within a 64 MiB read budget.
    #[arg(long, default_value_t = 1000)]
    pub(super) budget: usize,
}

#[derive(Args)]
pub struct ReplayArguments {
    pub(super) receipt: Id,
    /// Restrict replay to this snapshot. By default, read the receipt directly from the archive.
    #[arg(long)]
    pub(super) head: Option<Id>,
    /// Verify a separately supplied native format mapping from the selected snapshot. Repeat for multiple mappings.
    #[arg(long, requires = "head")]
    pub(super) mapping: Vec<Id>,
    /// Print the replay report and exit unsuccessfully if the receipt differs.
    #[arg(long)]
    pub(super) require_match: bool,
}

#[derive(Args)]
pub struct ReassessArguments {
    pub(super) receipt: Id,
    #[arg(long)]
    pub(super) head: Id,
    /// Add an argument to the saved query. Repeat for additional arguments.
    #[arg(long)]
    pub(super) argument: Vec<Id>,
    /// Add a native format mapping from the selected snapshot. Repeat for multiple mappings.
    #[arg(long)]
    pub(super) mapping: Vec<Id>,
    /// Override the saved query time for freshness checks.
    #[arg(long)]
    pub(super) at: Option<u64>,
}
