//! Display helpers for the cluster and url commands: plan and release choices,
//! `cluster show` rows, `history` lines, the rerun hint, and the progress of a
//! flow's Actions on stderr.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bonsai_api::{Action, ActionState, ActionType, Cluster, Plan, Release};
use bonsai_core::{ActionFailure, Event, EventSink};
use indicatif::ProgressBar;
use jiff::Timestamp;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use unicode_width::UnicodeWidthStr;

use crate::display::{group, plan_price, table_time, usage_rows, when};
use crate::output::{Column, Printer, TEXT_MIN, cell, human_bytes, relative_time, truncate};
use crate::sanitize::sanitize;

// ---------------------------------------------------------------- create's choices

/// The plans offered in `region`, in the API's order.
pub fn plans_in_region<'a>(plans: &'a [Plan], region: &str) -> Vec<&'a Plan> {
    plans
        .iter()
        .filter(|p| p.regions.iter().any(|r| r == region))
        .collect()
}

/// A plan as a prompt shows it: `Standard Fir — $20/mo`.
pub fn plan_choice(plan: &Plan) -> String {
    format!("{} — {}", plan.name, plan_price(plan))
}

/// The releases a plan offers in a region (its offer's) as a prompt shows them,
/// in offer order, with the release's name when the catalogue has it. The first
/// is the default.
pub fn release_choices(offered: &[String], releases: &[Release]) -> Vec<String> {
    offered
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let name = releases.iter().find(|r| &r.id == id).map(|r| &r.name);
            let mut label = match name {
                Some(name) => format!("{id} ({name})"),
                None => id.clone(),
            };
            if i == 0 {
                label.push_str(" — default");
            }
            label
        })
        .collect()
}

/// The flags a non-interactive create is missing: `--plan`, and `--region` unless
/// a `--space` names it.
pub fn missing_create_flags(
    plan: Option<&str>,
    region: Option<&str>,
    space: Option<&str>,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if plan.is_none() {
        missing.push("--plan");
    }
    if region.is_none() && space.is_none() {
        missing.push("--region");
    }
    missing
}

/// What a create was asked for, for the rerun hint.
pub struct CreateChoice<'a> {
    pub name: &'a str,
    pub plan: &'a str,
    pub region: &'a str,
    pub space: Option<&'a str>,
    pub release: Option<&'a str>,
}

/// `bonsai cluster create <name> --plan P --region R [--release V]` (or `--space S`
/// in place of `--region`): the command to run again. Never an idempotency key:
/// running it again is a new request.
pub fn rerun_hint(c: &CreateChoice<'_>) -> String {
    let mut hint = format!(
        "bonsai cluster create {} --plan {}",
        shell_quote(c.name),
        shell_quote(c.plan)
    );
    match c.space {
        Some(space) => hint.push_str(&format!(" --space {}", shell_quote(space))),
        None => hint.push_str(&format!(" --region {}", shell_quote(c.region))),
    }
    if let Some(release) = c.release {
        hint.push_str(&format!(" --release {}", shell_quote(release)));
    }
    hint
}

/// `s` as one POSIX shell word: as is when it's plain, otherwise single-quoted.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// What became of the cluster a failed create left, as `(outcome, cluster_id,
/// deprovision_action_id)`. The outcome is one of `removed`, `removed_by_bonsai`
/// (Bonsai had terminated it itself), `deprovisioning` (the removal started but
/// wasn't seen to finish), `remove_failed` (the `DELETE` or the deprovision
/// failed), `kept` (by the policy or the user) or `replayed` (a caller's key, so
/// maybe an earlier run's cluster). `None` when the failed Action left no cluster
/// this run knows of.
pub fn cleanup_outcome(f: &ActionFailure) -> Option<(&'static str, &str, Option<&str>)> {
    let id = f.cluster_id.as_deref()?;
    Some(match &f.cleanup {
        Some(c) => (
            c.outcome.as_str(),
            c.cluster_id.as_str(),
            c.outcome.deprovision_action_id(),
        ),
        None if f.replayed => ("replayed", id, None),
        None => ("kept", id, None),
    })
}

// ---------------------------------------------------------------- list and show

/// `cluster list`'s columns.
pub const LIST_COLUMNS: [Column; 8] = [
    Column::key("NAME"),
    Column::key("ID"),
    Column::fixed("STATE", 5),
    Column::fixed("PLAN", 4),
    Column::fixed("REGION", 3),
    Column::fixed("RELEASE", 1).wide(),
    Column::fixed("DOCS", 1).wide(),
    Column::fixed("SIZE", 2),
];

/// One `cluster list` row.
pub fn list_row(c: &Cluster) -> Vec<String> {
    vec![
        c.name.clone(),
        c.id.clone(),
        c.state.to_string(),
        c.plan.as_ref().map_or_else(|| "-".into(), |p| p.id.clone()),
        c.space.as_ref().map_or_else(
            || c.region.id.clone(),
            |s| format!("{} ({})", c.region.id, s.id),
        ),
        c.release.id.clone(),
        c.stats.docs.map_or_else(|| "-".into(), group),
        c.stats.bytes.map_or_else(|| "-".into(), human_bytes),
    ]
}

/// `Name (id)`.
fn named(name: &str, id: &str) -> String {
    format!("{name} ({id})")
}

/// `cluster show`'s key and value rows. No credentials: only `bonsai url` prints one.
pub fn show_rows(c: &Cluster, now: Timestamp) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Name", c.name.clone()),
        ("ID", c.id.clone()),
        ("State", c.state.to_string()),
        (
            "Plan",
            c.plan
                .as_ref()
                .map_or_else(|| "-".into(), |p| named(&p.name, &p.id)),
        ),
        ("Region", named(&c.region.name, &c.region.id)),
    ];
    if let Some(space) = &c.space {
        rows.push(("Space", named(&space.name, &space.id)));
    }
    rows.push(("Release", named(&c.release.name, &c.release.id)));
    rows.push((
        "Host",
        c.host.as_ref().map_or_else(
            || "-".into(),
            |h| match c.port {
                443 => h.clone(),
                port => format!("{h}:{port}"),
            },
        ),
    ));
    rows.extend(usage_rows(&c.stats));
    rows.extend([
        (
            "Latest action",
            c.latest_action.as_ref().map_or_else(
                || "-".into(),
                |a| format!("{} {} ({})", a.r#type, a.state, a.id),
            ),
        ),
        ("Created", when(c.created_at, now)),
    ]);
    if let Some(at) = c.terminated_at {
        rows.push(("Terminated", when(at, now)));
    }
    rows
}

// ---------------------------------------------------------------- history

/// `history`'s columns without `--steps`.
pub const HISTORY_COLUMNS: [Column; 6] = [
    Column::key("ACTION"),
    Column::fixed("TYPE", 5),
    Column::fixed("STATE", 4),
    Column::fixed("STARTED", 3),
    Column::text("ERROR", 2),
    // The full time, under --wide; dropped first, so it never pushes out STARTED.
    Column::fixed("STARTED AT", 1).wide(),
];

/// One `history` row: `STARTED` is relative, `STARTED AT` the UTC time.
pub fn history_row(a: &Action, now: Timestamp) -> Vec<String> {
    vec![
        a.id.clone(),
        a.r#type.to_string(),
        a.state.to_string(),
        relative_time(a.created_at, now),
        a.error
            .as_ref()
            .map_or_else(String::new, |e| e.code.to_string()),
        a.created_at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
    ]
}

/// One Action with its steps, for `history --steps`. API text is sanitized. With a
/// `width`, a step's message is cut with `…` so its line fits (to no fewer than
/// [`TEXT_MIN`] columns). Times are compact unless `wide`.
pub fn history_block(a: &Action, now: Timestamp, width: Option<usize>, wide: bool) -> Vec<String> {
    // Free text is cut to fit the width, to no fewer than TEXT_MIN columns.
    let fit = |text: String, taken: usize| match width {
        Some(w) => truncate(&text, w.saturating_sub(taken).max(TEXT_MIN)).into_owned(),
        None => text,
    };
    let mut head = format!(
        "{}  {}  {}  {}",
        cell(&a.id),
        cell(&a.r#type.to_string()),
        cell(&a.state.to_string()),
        table_time(a.created_at, now, wide)
    );
    if let Some(e) = &a.error {
        let error = fit(
            cell(&format!("{}: {}", e.code, e.message)),
            head.width() + 2,
        );
        head.push_str("  ");
        head.push_str(&error);
    }
    let mut lines = vec![head];
    lines.extend(a.steps.iter().map(|s| {
        let mark = format!("  {} ", step_mark(&s.state));
        let at = format!("  {}", table_time(s.at, now, wide));
        let message = fit(cell(&s.message), mark.width() + at.width());
        format!("{mark}{message}{at}")
    }));
    lines
}

/// The mark a finished step gets: `✗` for the one that failed, `✓` otherwise.
pub fn step_mark(state: &ActionState) -> &'static str {
    match state {
        ActionState::Error => "✗",
        _ => "✓",
    }
}

// ---------------------------------------------------------------- progress

/// What an Action does to its cluster, for the progress line.
fn doing(kind: &ActionType) -> String {
    match kind {
        ActionType::Provision => "Provisioning".into(),
        ActionType::Deprovision => "Removing".into(),
        ActionType::ChangePlan => "Changing the plan of".into(),
        other => format!("Running {other} on"),
    }
}

/// Renders a flow's [`Event`]s on stderr: a spinner with step lines above it on a
/// terminal, plain lines otherwise, nothing under `--json` or `--quiet`.
///
/// Cloning shares the state, so a prompt the flow asks mid-way (the failed-create
/// cleanup) can first show the steps so far and pause the spinner.
#[derive(Clone)]
pub struct Progress {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    out: Printer,
    rx: UnboundedReceiver<Event>,
    bar: Option<ProgressBar>,
    /// The cluster's name, for the progress line.
    label: String,
    /// The next Action started is the cleanup of a failed create.
    cleanup: bool,
}

impl Progress {
    /// A renderer for a flow on the cluster `label`, and the sink to hand the flow.
    pub fn new(json: bool, quiet: bool, label: &str) -> (Self, EventSink) {
        let (tx, rx) = unbounded_channel();
        let inner = Inner {
            out: Printer::new(json, quiet),
            rx,
            bar: None,
            label: label.to_owned(),
            cleanup: false,
        };
        let progress = Self {
            inner: Arc::new(Mutex::new(inner)),
        };
        (progress, tx)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renders every event sent so far.
    pub fn pump(&self) {
        let mut inner = self.lock();
        while let Ok(ev) = inner.rx.try_recv() {
            inner.handle(ev);
        }
    }

    /// Runs `f` (a prompt) with the spinner paused, after rendering what's pending.
    pub fn suspend<T>(&self, f: impl FnOnce() -> T) -> T {
        self.pump();
        let bar = self.lock().bar.clone();
        match bar {
            Some(bar) => bar.suspend(f),
            None => f(),
        }
    }

    /// Runs `fut`, rendering events as they come, then clears the spinner. Events
    /// are pumped on a short tick rather than awaited, so the lock is never held
    /// across an `.await` (a prompt inside `fut` takes it too).
    pub async fn drive<T>(&self, fut: impl Future<Output = T>) -> T {
        let mut fut = std::pin::pin!(fut);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let out = loop {
            tokio::select! {
                res = &mut fut => break res,
                _ = tick.tick() => self.pump(),
            }
        };
        self.pump();
        if let Some(bar) = self.lock().bar.take() {
            bar.finish_and_clear();
        }
        out
    }
}

/// `s` (sanitized) as a line of its own above the spinner: the spinner is cleared,
/// the line written to `out` with a real `\n`, and the spinner drawn again below it.
/// Not `ProgressBar::println`: indicatif 0.18 pads that line to the terminal's
/// width and relies on the terminal's autowrap for the line break, so on a terminal
/// whose width or wrapping differs from what indicatif assumes, every step runs
/// into the next.
fn line_above(bar: &ProgressBar, s: &str, out: &mut dyn std::io::Write) {
    bar.suspend(|| {
        let _ = writeln!(out, "{}", sanitize(s, false));
        let _ = out.flush();
    });
}

impl Inner {
    fn line(&self, s: &str) {
        if self.out.is_json() {
            return;
        }
        match &self.bar {
            Some(bar) => line_above(bar, s, &mut std::io::stderr()),
            None => self.out.info(s),
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::ActionStarted {
                action_id, kind, ..
            } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                let msg = if std::mem::take(&mut self.cleanup) {
                    format!("Removing the failed cluster {} ({action_id})", self.label)
                } else {
                    format!("{} {} ({action_id})", doing(&kind), self.label)
                };
                self.bar = self.out.spinner(&msg);
                if self.bar.is_none() {
                    self.line(&msg);
                }
            }
            Event::StepCompleted { message, state, .. } => {
                self.line(&format!("  {} {message}", step_mark(&state)));
            }
            Event::CleanupStarted { cluster_name, .. } => {
                self.label = cluster_name;
                self.cleanup = true;
            }
            Event::Retrying {
                attempt,
                wait,
                reason,
            } => self.line(&format!(
                "  retrying in {}s (attempt {attempt}): {reason}",
                wait.as_secs()
            )),
            // The outcome is reported once the flow returns.
            Event::CleanupFinished { .. } | Event::Waiting { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Offered with every one of `releases` in each of `regions`.
    fn plan(id: &str, cents: u64, regions: &[&str], releases: &[&str]) -> Plan {
        let offers: Vec<_> = regions
            .iter()
            .map(|r| json!({"region": r, "releases": releases}))
            .collect();
        serde_json::from_value(json!({
            "id": id,
            "name": format!("Plan {id}"),
            "price_in_cents": cents,
            "billing_interval_months": 1,
            "single_tenant": false,
            "private_network": false,
            "regions": regions,
            "releases": releases,
            "offers": offers,
        }))
        .unwrap()
    }

    fn release(id: &str, name: &str) -> Release {
        serde_json::from_value(json!({
            "id": id, "name": name, "engine": "opensearch", "version": "2.19.0"
        }))
        .unwrap()
    }

    #[test]
    fn plans_are_filtered_by_region_in_order() {
        let plans = vec![
            plan("a", 0, &["us", "eu"], &[]),
            plan("b", 100, &["eu"], &[]),
            plan("c", 200, &["us"], &[]),
        ];
        let ids = |r| -> Vec<String> {
            plans_in_region(&plans, r)
                .iter()
                .map(|p| p.id.clone())
                .collect()
        };
        assert_eq!(ids("us"), ["a", "c"]);
        assert_eq!(ids("eu"), ["a", "b"]);
        assert!(ids("ap").is_empty());
    }

    #[test]
    fn prices_read_like_a_price_list() {
        assert_eq!(plan_choice(&plan("s", 0, &[], &[])), "Plan s — Free");
        assert_eq!(plan_choice(&plan("s", 5000, &[], &[])), "Plan s — $50/mo");
        assert_eq!(
            plan_choice(&plan("s", 500_000, &[], &[])),
            "Plan s — $5,000/mo"
        );
    }

    #[test]
    fn the_first_release_is_the_default() {
        let offered = [
            "opensearch-3.8.0".to_owned(),
            "opensearch-2.19.6".to_owned(),
        ];
        let catalogue = [release("opensearch-3.8.0", "OpenSearch 3.8.0")];
        assert_eq!(
            release_choices(&offered, &catalogue),
            [
                "opensearch-3.8.0 (OpenSearch 3.8.0) — default",
                "opensearch-2.19.6",
            ]
        );
        assert!(release_choices(&[], &catalogue).is_empty());
    }

    #[test]
    fn missing_flags_name_plan_and_region() {
        assert_eq!(
            missing_create_flags(None, None, None),
            ["--plan", "--region"]
        );
        assert_eq!(missing_create_flags(Some("p"), None, None), ["--region"]);
        assert!(missing_create_flags(Some("p"), None, Some("space")).is_empty());
        assert_eq!(missing_create_flags(None, Some("r"), None), ["--plan"]);
    }

    #[test]
    fn the_rerun_hint_never_carries_a_key() {
        let mut c = CreateChoice {
            name: "search-prod",
            plan: "standard-fir",
            region: "aws-us-east-1",
            space: None,
            release: None,
        };
        assert_eq!(
            rerun_hint(&c),
            "bonsai cluster create search-prod --plan standard-fir --region aws-us-east-1"
        );
        c.release = Some("opensearch-3.8.0");
        c.name = "it's mine";
        assert_eq!(
            rerun_hint(&c),
            r"bonsai cluster create 'it'\''s mine' --plan standard-fir --region aws-us-east-1 --release opensearch-3.8.0"
        );
        c.space = Some("acme-private-use1");
        assert!(rerun_hint(&c).contains("--space acme-private-use1"));
        assert!(!rerun_hint(&c).contains("--region"));
        assert!(!rerun_hint(&c).contains("idempotency"));
    }

    #[test]
    fn shell_quote_leaves_plain_words_alone() {
        assert_eq!(shell_quote("ci-1"), "ci-1");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$(rm)"), "'$(rm)'");
    }

    #[test]
    fn history_steps_truncate_to_the_width() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        let a: Action = serde_json::from_value(json!({
            "id": "act_1",
            "type": "cluster.provision",
            "state": "success",
            "subject": {"type": "cluster", "id": "c1"},
            "created_at": "2026-10-20T11:00:00Z",
            "finished_at": null,
            "error": null,
            "result": null,
            "steps": [
                {"state": "success", "message": "Reserving capacity on a node with plenty of room", "at": "2026-10-20T11:00:00Z"},
                {"state": "success", "message": "Done", "at": "2026-10-20T11:00:01Z"},
            ],
        }))
        .unwrap();
        // Compact times by default; --wide has the full one.
        let whole = history_block(&a, now, None, false);
        assert_eq!(whole[0], "act_1  cluster.provision  success  1h ago");
        assert_eq!(
            whole[1],
            "  ✓ Reserving capacity on a node with plenty of room  1h ago"
        );
        let wide = history_block(&a, now, None, true);
        assert_eq!(
            wide[0],
            "act_1  cluster.provision  success  2026-10-20 11:00:00 UTC (1h ago)"
        );
        assert!(
            wide[1].ends_with("plenty of room  2026-10-20 11:00:00 UTC (1h ago)"),
            "{wide:?}"
        );
        // At 60 the compact line fits whole; at 40 the message is cut.
        assert_eq!(history_block(&a, now, Some(60), false)[1], whole[1]);
        let cut = history_block(&a, now, Some(40), false);
        assert_eq!(cut[1], "  ✓ Reserving capacity on a nod…  1h ago");
        assert_eq!(cut[1].chars().count(), 40);
        assert_eq!(cut[2], whole[2]);
        // Never narrower than the minimum, however small the width.
        let tiny = history_block(&a, now, Some(10), false);
        assert!(tiny[1].starts_with("  ✓ Reserving c…  "), "{tiny:?}");
    }

    #[test]
    fn history_steps_are_one_line_and_errors_fit() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        let a: Action = serde_json::from_value(json!({
            "id": "act_1",
            "type": "cluster.provision",
            "state": "error",
            "subject": {"type": "cluster", "id": "c1"},
            "created_at": "2026-10-20T11:00:00Z",
            "finished_at": "2026-10-20T11:00:01Z",
            "error": {"code": "provisioning_failed", "message": "the node\nran out of\tdisk space while copying shards"},
            "result": null,
            "steps": [
                {"state": "error", "message": "Copying\nshards\tnow", "at": "2026-10-20T11:00:00Z"},
            ],
        }))
        .unwrap();
        // As the steps table would show them: tabs and newlines become spaces.
        let whole = history_block(&a, now, None, false);
        assert_eq!(
            whole[0],
            "act_1  cluster.provision  error  1h ago  provisioning_failed: the node ran out of disk space while copying shards"
        );
        assert_eq!(whole[1], "  ✗ Copying shards now  1h ago");
        let cut = history_block(&a, now, Some(80), false);
        assert_eq!(cut[0].width(), 80, "{cut:?}");
        assert!(cut[0].ends_with('…'), "{cut:?}");
        assert!(
            cut[0].starts_with(
                "act_1  cluster.provision  error  1h ago  provisioning_failed: the node"
            ),
            "{cut:?}"
        );
    }

    #[test]
    fn history_rows_have_a_compact_and_a_wide_time() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        let a: Action = serde_json::from_value(json!({
            "id": "act_1",
            "type": "cluster.provision",
            "state": "success",
            "subject": {"type": "cluster", "id": "c1"},
            "created_at": "2026-10-17T12:00:00Z",
            "finished_at": null,
            "error": null,
            "result": null,
            "steps": [],
        }))
        .unwrap();
        let row = history_row(&a, now);
        assert_eq!(HISTORY_COLUMNS[3].header, "STARTED");
        assert_eq!(row[3], "3d ago");
        // --wide adds the full time as its own column, dropped first.
        let at = HISTORY_COLUMNS
            .iter()
            .position(|c| c.header == "STARTED AT")
            .unwrap();
        assert!(HISTORY_COLUMNS[at].wide);
        let lowest = HISTORY_COLUMNS.iter().filter_map(|c| c.priority).min();
        assert_eq!(HISTORY_COLUMNS[at].priority, lowest);
        assert_eq!(
            HISTORY_COLUMNS
                .iter()
                .filter(|c| c.priority == lowest)
                .count(),
            1
        );
        assert_eq!(row[at], "2026-10-17 12:00:00 UTC");
    }

    /// A terminal that records what's drawn to it, shared with the writer the step
    /// lines go to, so the test sees one byte stream in order.
    #[derive(Clone, Debug, Default)]
    struct Tape(Arc<Mutex<Vec<u8>>>);

    impl Tape {
        fn push(&self, s: &str) {
            self.0.lock().unwrap().extend_from_slice(s.as_bytes());
        }
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for Tape {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl indicatif::TermLike for Tape {
        fn width(&self) -> u16 {
            60
        }
        fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
            if n > 0 {
                self.push(&format!("<up{n}>"));
            }
            Ok(())
        }
        fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
            if n > 0 {
                self.push(&format!("<down{n}>"));
            }
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> std::io::Result<()> {
            self.push(&format!("{s}\n"));
            Ok(())
        }
        fn write_str(&self, s: &str) -> std::io::Result<()> {
            self.push(s);
            Ok(())
        }
        fn clear_line(&self) -> std::io::Result<()> {
            self.push("<clear>");
            Ok(())
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_step_line_above_the_spinner_ends_in_a_newline() {
        let tape = Tape::default();
        let bar = ProgressBar::with_draw_target(
            None,
            indicatif::ProgressDrawTarget::term_like(Box::new(tape.clone())),
        );
        bar.set_style(indicatif::ProgressStyle::with_template("{spinner} {msg}").unwrap());
        bar.set_message("Provisioning t1");
        bar.tick();
        let mut out = tape.clone();
        line_above(&bar, "  ✓ Reserving capacity", &mut out);
        line_above(&bar, "  ✓ Provisioning\rcluster", &mut out);
        bar.finish_and_clear();
        let text = tape.text();
        // Each step is its own line, cleared of the spinner first and not padded
        // to the terminal's width (indicatif's println relies on autowrap).
        assert!(text.contains("<clear>  ✓ Reserving capacity\n"), "{text:?}");
        assert!(
            text.contains("<clear>  ✓ Provisioning\u{FFFD}cluster\n"),
            "{text:?}"
        );
        assert!(!text.contains("capacity "), "padded: {text:?}");
        // The spinner is cleared at the end, not left on the screen.
        assert!(text.ends_with("<clear>"), "{text:?}");
    }
}
