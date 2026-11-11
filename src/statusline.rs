//! Compact one-line status output for agent hooks.
use crate::agents::AgentReader;
use crate::pricing::Pricing;
use crate::report::{self, Ctx};
use crate::types::*;
use std::io::{IsTerminal, Read};

pub fn run(readers: &[Box<dyn AgentReader>], pricing: &Pricing, tz: &jiff::tz::TimeZone, json: bool) {
    // optional hook payload on stdin (non-tty)
    let mut stdin_buf = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    }
    let payload = serde_json::from_str::<serde_json::Value>(&stdin_buf).unwrap_or_default();
    let model = payload
        .pointer("/model/display_name")
        .or_else(|| payload.pointer("/model/id"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let mut entries: Vec<UsageEntry> = readers.iter().flat_map(|r| r.entries()).collect();
    entries = crate::agents::dedup(entries);

    let now = jiff::Timestamp::now().as_millisecond();
    let ctx = Ctx {
        pricing,
        mode: report::CostMode::Auto,
        tz: tz.clone(),
        merged: true,
        by_agent: false,
        no_cost: false,
    };

    // today's cost
    let today_key = report::date_key(now, tz);
    let mut today = Acc::default();
    for e in &entries {
        if report::date_key(e.timestamp_ms, tz) == today_key {
            today.add(e, pricing);
        }
    }
    let mut today_cost = 0.0;
    for (m, agg) in &today.models {
        today_cost += ctx.cost(m, agg).0;
    }

    // active block
    let bl = report::blocks(&entries, 5, now, pricing, tz);
    let active = bl.iter().find(|b| b.is_active);
    let (burn_rate, remaining) = match active {
        Some(b) => {
            let elapsed_h = (now - b.start_ms).max(1) as f64 / 3_600_000.0;
            let mut cost = 0.0;
            for (m, agg) in &b.acc.models {
                cost += ctx.cost(m, agg).0;
            }
            (cost / elapsed_h, (b.end_ms - now).max(0))
        }
        None => (0.0, 0),
    };

    if json {
        let mut block_end = String::new();
        if let Some(b) = active {
            block_end = report::ms_to_iso(b.end_ms);
        }
        println!(
            "{}",
            serde_json::json!({
                "model": model,
                "todayCost": today_cost,
                "burnRatePerHour": burn_rate,
                "blockRemainingMs": remaining,
                "blockEnd": block_end,
            })
        );
        return;
    }

    let rem_h = remaining / 3_600_000;
    let rem_m = (remaining % 3_600_000) / 60_000;
    let mut parts = vec![];
    if !model.is_empty() {
        parts.push(format!("🤖 {model}"));
    }
    parts.push(format!("💰 ${:.2} today", today_cost));
    if active.is_some() {
        parts.push(format!("🔥 ${:.2}/hr", burn_rate));
        parts.push(format!("⏱ {}h {:02}m left", rem_h, rem_m));
    }
    println!("{}", parts.join(" | "));
}
