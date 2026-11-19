//! Where the time goes between a gRPC update and a landed liquidation, and which detection path
//! started it.
//!
//! Every liquidation put in the queue carries an [`AttemptTrace`]: the path that detected it (the hot
//! path, the background path after an update, the sweep, or the startup scan), when the update behind
//! it arrived off the network, and when it was queued. The worker and the executor mark the stages
//! after that as they pass them:
//!
//! ```text
//! grpc arrival -> detected (queued) -> taken by a worker -> re-evaluated -> plan built
//!              -> submitted -> landed
//! ```
//!
//! One `LIQUIDATION_TIMELINE` line is logged per attempt with every stage's duration in
//! milliseconds, and a per-minute `EXEC_TIMING` line per detection path gives percentiles, so the
//! paths can be compared on the same liquidations' whole journey.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use log::info;
use solana_pubkey::Pubkey;

/// What detected a liquidation, and so what put it in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DetectionPath {
    /// The update path, checking the hot list as an update arrived.
    Hot,
    /// The background path, evaluating the accounts an update touched.
    Background,
    /// The rolling full sweep of every borrower.
    Sweep,
    /// The scan of every borrower at startup.
    Startup,
    /// Built outside the queue (a test, a placeholder).
    Untraced,
}

impl DetectionPath {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionPath::Hot => "hot",
            DetectionPath::Background => "background",
            DetectionPath::Sweep => "sweep",
            DetectionPath::Startup => "startup",
            DetectionPath::Untraced => "untraced",
        }
    }
}

/// The stages after detection that are marked as the attempt passes them.
#[derive(Clone, Copy, Debug)]
pub enum Stage {
    /// A worker took the liquidation off the queue.
    Taken,
    /// The worker re-sized it on the current state.
    Evaluated,
    /// The transaction (or bundle) is built and about to go out: any quote has been waited for.
    Planned,
    /// A relay (or the RPC) accepted it.
    Submitted,
    /// It confirmed on chain.
    Landed,
}

#[derive(Default, Clone, Copy)]
struct Marks {
    taken: Option<Instant>,
    evaluated: Option<Instant>,
    planned: Option<Instant>,
    submitted: Option<Instant>,
    landed: Option<Instant>,
}

/// How long each stage took, where both of its ends were marked.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Stages {
    pub grpc_to_detected: Option<Duration>,
    pub queue_wait: Option<Duration>,
    pub evaluate: Option<Duration>,
    pub plan: Option<Duration>,
    pub submit: Option<Duration>,
    pub land: Option<Duration>,
    /// From the update's arrival (or, without one, from detection) to submitted.
    pub to_submitted: Option<Duration>,
    /// From the update's arrival (or detection) to the last stage marked.
    pub total: Option<Duration>,
}

pub struct AttemptTrace {
    pub path: DetectionPath,
    /// When the oldest update behind the detection arrived from the network.
    pub arrival: Option<Instant>,
    /// When the liquidation was put in the queue.
    pub detected: Instant,
    /// The attempt id the detection logged (`HOT_PATH_FOUND`, `BACKGROUND_FOUND`), which the
    /// worker's own re-evaluation does not keep.
    pub detected_attempt_id: u64,
    marks: Mutex<Marks>,
    details: Mutex<Vec<(&'static str, String)>>,
    reported: AtomicBool,
}

impl AttemptTrace {
    pub fn new(
        path: DetectionPath,
        arrival: Option<Instant>,
        detected_attempt_id: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            arrival,
            detected: Instant::now(),
            detected_attempt_id,
            marks: Mutex::new(Marks::default()),
            details: Mutex::new(Vec::new()),
            reported: AtomicBool::new(false),
        })
    }

    /// For an intent that was never queued by a detection path.
    pub fn untraced() -> Arc<Self> {
        Self::new(DetectionPath::Untraced, None, 0)
    }

    /// Marks `stage` as reached now. The first time wins: a retry on another plan does not move it.
    pub fn mark(&self, stage: Stage) {
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        let slot = match stage {
            Stage::Taken => &mut marks.taken,
            Stage::Evaluated => &mut marks.evaluated,
            Stage::Planned => &mut marks.planned,
            Stage::Submitted => &mut marks.submitted,
            Stage::Landed => &mut marks.landed,
        };
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// Records a detail of the attempt to print with its timeline (the plan, the tip, the relay).
    pub fn note(&self, key: &'static str, value: impl ToString) {
        let mut details = self.details.lock().unwrap_or_else(|e| e.into_inner());
        let value = value.to_string();
        match details.iter_mut().find(|(known, _)| *known == key) {
            Some(entry) => entry.1 = value,
            None => details.push((key, value)),
        }
    }

    pub fn landed(&self) -> bool {
        self.marks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .landed
            .is_some()
    }

    pub fn stages(&self) -> Stages {
        let marks = *self.marks.lock().unwrap_or_else(|e| e.into_inner());
        let between = |from: Option<Instant>, to: Option<Instant>| match (from, to) {
            (Some(from), Some(to)) => Some(to.saturating_duration_since(from)),
            _ => None,
        };
        let start = self.arrival.unwrap_or(self.detected);
        let last = [
            marks.landed,
            marks.submitted,
            marks.planned,
            marks.evaluated,
            marks.taken,
        ]
        .into_iter()
        .flatten()
        .next()
        .unwrap_or(self.detected);
        Stages {
            grpc_to_detected: self
                .arrival
                .map(|arrival| self.detected.saturating_duration_since(arrival)),
            queue_wait: between(Some(self.detected), marks.taken),
            evaluate: between(marks.taken, marks.evaluated),
            plan: between(marks.evaluated, marks.planned),
            submit: between(marks.planned, marks.submitted),
            land: between(marks.submitted, marks.landed),
            to_submitted: between(Some(start), marks.submitted),
            total: Some(last.saturating_duration_since(start)),
        }
    }

    /// Logs the attempt's timeline once, whichever of the places that can end it gets there first,
    /// and adds it to the per-path summary. `outcome` is `landed`, `lost`, `not_sent`, `skipped`,
    /// `failed` and so on.
    pub fn report(&self, attempt_id: u64, account: &Pubkey, outcome: &str) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let stages = self.stages();
        let ms = |duration: Option<Duration>| {
            duration.map_or_else(|| "-".to_string(), |d| d.as_millis().to_string())
        };
        let details: String = self
            .details
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(key, value)| format!(" {key}={value}"))
            .collect();
        info!(
            "LIQUIDATION_TIMELINE attempt_id={} detected_as={} account={} path={} outcome={} grpc_to_detected_ms={} queue_wait_ms={} evaluate_ms={} plan_ms={} submit_ms={} land_ms={} grpc_to_submitted_ms={} total_ms={}{}",
            attempt_id,
            self.detected_attempt_id,
            account,
            self.path.as_str(),
            outcome,
            ms(stages.grpc_to_detected),
            ms(stages.queue_wait),
            ms(stages.evaluate),
            ms(stages.plan),
            ms(stages.submit),
            ms(stages.land),
            ms(stages.to_submitted),
            ms(stages.total),
            details
        );
        record(self.path, outcome, &stages);
    }
}

/// What one detection path's attempts did since the last summary.
#[derive(Default)]
struct PathStats {
    attempts: usize,
    outcomes: HashMap<String, usize>,
    grpc_to_detected: Vec<u64>,
    queue_wait: Vec<u64>,
    evaluate: Vec<u64>,
    plan: Vec<u64>,
    submit: Vec<u64>,
    land: Vec<u64>,
    to_submitted: Vec<u64>,
    total: Vec<u64>,
}

fn summary() -> &'static Mutex<HashMap<DetectionPath, PathStats>> {
    static SUMMARY: OnceLock<Mutex<HashMap<DetectionPath, PathStats>>> = OnceLock::new();
    SUMMARY.get_or_init(Default::default)
}

fn record(path: DetectionPath, outcome: &str, stages: &Stages) {
    let mut summary = summary().lock().unwrap_or_else(|e| e.into_inner());
    let stats = summary.entry(path).or_default();
    stats.attempts += 1;
    *stats.outcomes.entry(outcome.to_string()).or_default() += 1;
    let push = |into: &mut Vec<u64>, from: Option<Duration>| {
        if let Some(duration) = from {
            into.push(duration.as_millis() as u64);
        }
    };
    push(&mut stats.grpc_to_detected, stages.grpc_to_detected);
    push(&mut stats.queue_wait, stages.queue_wait);
    push(&mut stats.evaluate, stages.evaluate);
    push(&mut stats.plan, stages.plan);
    push(&mut stats.submit, stages.submit);
    push(&mut stats.land, stages.land);
    push(&mut stats.to_submitted, stages.to_submitted);
    push(&mut stats.total, stages.total);
}

/// `p50/p90/max` of `values` in ms, or `-` when there are none.
fn spread(values: &[u64]) -> String {
    if values.is_empty() {
        return "-".to_string();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q).round() as usize];
    format!("{}/{}/{}", at(0.5), at(0.9), at(1.0))
}

fn render(path: DetectionPath, stats: &PathStats) -> String {
    let mut outcomes: Vec<(&String, &usize)> = stats.outcomes.iter().collect();
    outcomes.sort();
    let outcomes: Vec<String> = outcomes
        .into_iter()
        .map(|(outcome, count)| format!("{outcome}={count}"))
        .collect();
    format!(
        "EXEC_TIMING path={} attempts={} outcomes=[{}] (ms, p50/p90/max) grpc_to_detected={} queue_wait={} evaluate={} plan={} submit={} land={} grpc_to_submitted={} total={}",
        path.as_str(),
        stats.attempts,
        outcomes.join(" "),
        spread(&stats.grpc_to_detected),
        spread(&stats.queue_wait),
        spread(&stats.evaluate),
        spread(&stats.plan),
        spread(&stats.submit),
        spread(&stats.land),
        spread(&stats.to_submitted),
        spread(&stats.total),
    )
}

/// One summary line for each detection path that finished attempts since the last call.
pub fn take_summary() -> Vec<String> {
    let mut summary = summary().lock().unwrap_or_else(|e| e.into_inner());
    let mut paths: Vec<(DetectionPath, PathStats)> = summary.drain().collect();
    paths.sort_by_key(|(path, _)| path.as_str());
    paths
        .iter()
        .map(|(path, stats)| render(*path, stats))
        .collect()
}

thread_local! {
    /// The attempt this thread is executing, so the stages passed deep in the executor can be marked
    /// without passing the trace through every call.
    static CURRENT: RefCell<Option<Arc<AttemptTrace>>> = const { RefCell::new(None) };
}

/// Makes `trace` the one this thread marks until the guard is dropped.
pub struct TraceGuard {
    previous: Option<Arc<AttemptTrace>>,
}

pub fn enter(trace: Arc<AttemptTrace>) -> TraceGuard {
    let previous = CURRENT.with(|current| current.replace(Some(trace)));
    TraceGuard { previous }
}

impl Drop for TraceGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Marks `stage` on the attempt this thread is executing, if any.
pub fn mark(stage: Stage) {
    CURRENT.with(|current| {
        if let Some(trace) = current.borrow().as_ref() {
            trace.mark(stage);
        }
    });
}

/// Records a detail on the attempt this thread is executing, if any.
pub fn note(key: &'static str, value: impl ToString) {
    CURRENT.with(|current| {
        if let Some(trace) = current.borrow().as_ref() {
            trace.note(key, value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn pause() {
        thread::sleep(Duration::from_millis(3));
    }

    #[test]
    fn stages_are_measured_between_the_marks_and_missing_ones_are_left_out() {
        let arrival = Instant::now();
        pause();
        let trace = AttemptTrace::new(DetectionPath::Hot, Some(arrival), 7);
        pause();
        trace.mark(Stage::Taken);
        pause();
        trace.mark(Stage::Evaluated);
        pause();
        trace.mark(Stage::Planned);
        pause();
        trace.mark(Stage::Submitted);

        let stages = trace.stages();
        assert!(stages.grpc_to_detected.unwrap() >= Duration::from_millis(3));
        assert!(stages.queue_wait.unwrap() >= Duration::from_millis(3));
        assert!(stages.evaluate.unwrap() >= Duration::from_millis(3));
        assert!(stages.plan.unwrap() >= Duration::from_millis(3));
        assert!(stages.submit.unwrap() >= Duration::from_millis(3));
        assert_eq!(stages.land, None, "it has not landed");
        // Counted from the update's arrival, not from detection.
        assert!(stages.to_submitted.unwrap() >= stages.grpc_to_detected.unwrap());
        assert_eq!(stages.total, stages.to_submitted, "the last stage marked");

        pause();
        trace.mark(Stage::Landed);
        assert!(trace.landed());
        assert!(trace.stages().land.unwrap() >= Duration::from_millis(3));
        assert!(trace.stages().total.unwrap() > stages.total.unwrap());
    }

    #[test]
    fn a_stage_keeps_the_first_time_it_was_marked() {
        let trace = AttemptTrace::untraced();
        trace.mark(Stage::Planned);
        let first = trace.stages();
        pause();
        // A second plan after the first failed does not move it.
        trace.mark(Stage::Planned);
        trace.mark(Stage::Submitted);
        assert_eq!(trace.stages().plan, first.plan);
        assert_eq!(
            first.grpc_to_detected, None,
            "an untraced intent has no arrival"
        );
    }

    #[test]
    fn the_trace_marked_through_the_thread_is_the_one_entered() {
        let trace = AttemptTrace::untraced();
        {
            let _guard = enter(trace.clone());
            mark(Stage::Taken);
            note("plan", "sandwich");
            // A nested attempt takes over and gives it back.
            let inner = AttemptTrace::untraced();
            {
                let _inner = enter(inner.clone());
                mark(Stage::Evaluated);
                mark(Stage::Planned);
            }
            mark(Stage::Submitted);
            assert!(inner.stages().plan.is_some());
        }
        // Outside the guard nothing is marked.
        mark(Stage::Landed);
        assert!(!trace.landed());
        let stages = trace.stages();
        assert!(stages.queue_wait.is_some());
        assert!(stages.plan.is_none(), "the inner attempt's mark");
        assert!(trace
            .details
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| *k == "plan" && v == "sandwich"));
    }

    #[test]
    fn an_attempt_is_reported_once() {
        let trace = AttemptTrace::new(DetectionPath::Startup, None, 1);
        let account = Pubkey::new_unique();
        trace.report(1, &account, "failed");
        assert!(trace.reported.load(Ordering::SeqCst));
        // A second ending (the watcher after the worker) changes nothing.
        let before = summary()
            .lock()
            .unwrap()
            .get(&DetectionPath::Startup)
            .map(|s| s.attempts);
        trace.report(1, &account, "landed");
        let after = summary()
            .lock()
            .unwrap()
            .get(&DetectionPath::Startup)
            .map(|s| s.attempts);
        assert_eq!(before, after);
    }

    #[test]
    fn the_summary_line_gives_percentiles_by_path() {
        let mut stats = PathStats::default();
        for (outcome, total) in [("landed", 100u64), ("landed", 200), ("lost", 900)] {
            stats.attempts += 1;
            *stats.outcomes.entry(outcome.to_string()).or_default() += 1;
            stats.total.push(total);
        }
        stats.grpc_to_detected.push(5);
        let line = render(DetectionPath::Hot, &stats);
        assert!(line.starts_with("EXEC_TIMING path=hot attempts=3 outcomes=[landed=2 lost=1]"));
        assert!(line.contains("grpc_to_detected=5/5/5"));
        assert!(line.contains("total=200/900/900"), "{line}");
        assert!(line.contains("plan=-"), "a stage nobody reached");
        assert_eq!(spread(&[]), "-");
        assert_eq!(spread(&[10, 20, 30, 40, 50]), "30/50/50");
    }
}
