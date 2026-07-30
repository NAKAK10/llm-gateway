//! `llm-gateway stats` — what was spent, and on what.
//!
//! Reads `usage-*.jsonl` and aggregates. Token counts are always meaningful;
//! money is not always available, because a flat-rate subscription has no
//! per-token price. So the table always shows tokens and shows cost only when
//! something computed one.

use clap::ValueEnum;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GroupBy {
    Route,
    Client,
    Provider,
    Model,
    Day,
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Route => "route",
            Self::Client => "client",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Day => "day",
        };
        f.write_str(s)
    }
}

pub struct Options {
    pub by: GroupBy,
    /// `YYYY-MM-DD`, inclusive.
    pub since: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    pub until: Option<String>,
}

/// One aggregated row.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub key: String,
    pub calls: u64,
    pub failures: u64,
    pub in_tok: u64,
    pub out_tok: u64,
}

pub fn run(options: Options) -> Result<()> {
    let _ = options;
    todo!("src/cli/stats.rs")
}

/// Aggregate already-parsed records. Pure, so it can be tested directly.
pub fn aggregate(
    records: &[crate::record::usage_log::UsageRecord],
    by: GroupBy,
    since: Option<&str>,
    until: Option<&str>,
) -> Vec<Row> {
    let _ = (records, by, since, until);
    todo!("src/cli/stats.rs")
}
