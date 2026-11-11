//! Compact one-line status output for agent hooks.
use crate::agents::AgentReader;
use crate::cli::DEFAULT_SESSION_HOURS;
use crate::pricing::Pricing;
use crate::report::{self, CostMode, Ctx};
use crate::types::*;
use std::io::{IsTerminal, Read};

/// A hook payload may arrive on stdin when invoked by the agent harness.
/// Read it with a bound: when stdin is a held-open pipe (no data, no EOF),
/// blocking on read_to_string would hang forever, so give up quickly.
fn hook_payload() -> serde_json::Value {
    if std::io::stdin().is_terminal() {
        return serde_json::Value::Null;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let buf = rx
        .recv_timeout(std::time::Duration::from_millis(50))
        .unwrap_or_default();
    serde_json::from_str(&buf).unwrap_or_default()
}

pub fn run(
    readers: &[Box<dyn AgentReader>],
    pricing: &Pricing,
    tz: &jiff::tz::TimeZone,
    json: bool,
) {
    let payload = hook_payload();
    let model = payload
        .pointer("/model/display_name")
        .or_else(|| payload.pointer("/model/id"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let entries = crate::app::load_entries(readers);

    let now = jiff::Timestamp::now().as_millisecond();
    // statusline never buckets by week — the sow value is irrelevant here
    let ctx = Ctx::new(pricing, CostMode::Auto, tz.clone(), true, false, 1);

    let today_key = report::date_key(now, tz);
    let mut today = Acc::default();
    for e in &entries {
        if report::date_key(e.timestamp_ms, tz) == today_key {
            today.add(e, pricing);
        }
    }
    let today_cost = ctx.acc_cost(&today);

    // statusline burn rate is cost over wall-clock block elapsed (differs
    // from block_row's first→last-entry span — that's upstream's formula)
    let bl = report::blocks(&entries, DEFAULT_SESSION_HOURS, now, pricing);
    let active = bl.iter().find(|b| b.is_active);
    let (burn_rate, remaining) = match active {
        Some(b) => {
            let elapsed_h = (now - b.start_ms).max(1) as f64 / 3_600_000.0;
            (ctx.acc_cost(&b.acc) / elapsed_h, (b.end_ms - now).max(0))
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
