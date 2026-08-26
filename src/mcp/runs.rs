//! Background load-test runs and their report artifacts.
//!
//! A load test can run far longer than a client will hold a request open, and
//! a client that times out is expected to cancel. Cancelling a flow that has
//! already submitted transactions loses the record of money already spent, so
//! these runs detach: starting one returns an identifier, and the report is
//! read back once it lands.
//!
//! Finished runs persist a JSON report named after their identifier, so the
//! artifact on disk is the store. This registry tracks only what is still in
//! flight, which is why a completed run survives a restart and an in-flight
//! one does not.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::task::JoinHandle;

/// What a caller learns about a run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RunState {
    /// Still executing in this process.
    Running { run_id: String },
    /// Finished, with its report.
    Finished {
        run_id: String,
        report: serde_json::Value,
    },
    /// No report, and not running here. Either it failed before writing one,
    /// or it was started by a server that has since restarted. Deliberately
    /// distinct from running: a caller must not read "no report yet" as
    /// "still working".
    Unknown { run_id: String },
}

/// Tracks load-test runs started through this server.
#[derive(Clone)]
pub struct RunRegistry {
    reports_dir: PathBuf,
    in_flight: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl RunRegistry {
    pub fn new(reports_dir: PathBuf) -> Self {
        Self {
            reports_dir,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Mint an identifier for a new run.
    ///
    /// Seconds-since-epoch keeps identifiers sortable, so listing newest-first
    /// is a reverse sort rather than a stat of every file.
    pub fn new_run_id() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("axe-load-test-{secs}")
    }

    /// Run a flow on its own thread, with its own runtime, and record it as in
    /// flight.
    ///
    /// Deliberately not `tokio::spawn`: the load-test future is not `Send`, so
    /// it cannot be moved onto the server's runtime. Building it inside a
    /// fresh thread means it is created and polled in one place and never
    /// crosses a thread boundary, which is what removes the `Send`
    /// requirement. The cost is one thread and one runtime per run, which is
    /// acceptable for a flow that runs for minutes.
    ///
    /// `make_flow` is a closure rather than a future for the same reason: the
    /// future must not exist until it is on the thread that will poll it.
    pub fn spawn_blocking_flow<M, F>(&self, run_id: &str, make_flow: M)
    where
        M: FnOnce() -> F + Send + 'static,
        F: Future<Output = ()>,
    {
        let handle = tokio::task::spawn_blocking(move || {
            // Nothing to report a build failure to: the caller already holds
            // its identifier and will see the run as unknown, which is
            // accurate.
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(make_flow());
        });

        if let Ok(mut runs) = self.in_flight.lock() {
            runs.insert(run_id.to_string(), handle);
        }
    }

    /// Whether a run is still executing in this process.
    fn is_running(&self, run_id: &str) -> bool {
        self.in_flight
            .lock()
            .is_ok_and(|runs| runs.get(run_id).is_some_and(|h| !h.is_finished()))
    }

    /// The state of one run, reading its artifact if it has landed.
    pub async fn state(&self, run_id: &str) -> RunState {
        if let Some(report) = self.read_report(run_id).await {
            return RunState::Finished {
                run_id: run_id.to_string(),
                report,
            };
        }
        if self.is_running(run_id) {
            return RunState::Running {
                run_id: run_id.to_string(),
            };
        }
        RunState::Unknown {
            run_id: run_id.to_string(),
        }
    }

    async fn read_report(&self, run_id: &str) -> Option<serde_json::Value> {
        let path = self.reports_dir.join(format!("{run_id}.json"));
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Known runs, newest first.
    ///
    /// Reports outlive the process, so this finds runs from earlier sessions
    /// too. Keying off the artifact rather than memory is the point.
    pub async fn list(&self) -> Vec<serde_json::Value> {
        let mut ids = Vec::new();

        if let Ok(mut entries) = tokio::fs::read_dir(&self.reports_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(id) = entry.file_name().to_string_lossy().strip_suffix(".json") {
                    ids.push(id.to_string());
                }
            }
        }

        if let Ok(runs) = self.in_flight.lock() {
            for id in runs.keys() {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }

        ids.sort_unstable();
        ids.reverse();

        ids.into_iter()
            .map(|id| {
                let running = self.is_running(&id);
                serde_json::json!({
                    "run_id": id,
                    "state": if running { "running" } else { "finished" },
                })
            })
            .collect()
    }
}
