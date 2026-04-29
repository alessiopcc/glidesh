use glidesh::modules::host::HostOutput;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TaskKey {
    pub step_idx: usize,
    pub task_idx: usize,
    pub loop_iter: usize,
}

type HostCell = Arc<OnceCell<Result<HostOutput, String>>>;

/// Coordinates one-shot execution of `host` tasks across parallel NodeRunners.
/// The first runner to reach a given TaskKey executes the closure; other
/// runners await the cached result.
pub struct HostCoordinator {
    cells: Mutex<HashMap<TaskKey, HostCell>>,
}

impl Default for HostCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl HostCoordinator {
    pub fn new() -> Self {
        Self {
            cells: Mutex::new(HashMap::new()),
        }
    }

    fn cell_for(&self, key: TaskKey) -> HostCell {
        let mut guard = self.cells.lock().expect("HostCoordinator mutex poisoned");
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }

    /// First caller executes `f`; concurrent/later callers receive a clone of
    /// the cached result. Errors are stored and broadcast identically.
    pub async fn get_or_run<F, Fut>(&self, key: TaskKey, f: F) -> Result<HostOutput, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<HostOutput, String>>,
    {
        let cell = self.cell_for(key);
        let stored = cell.get_or_init(|| async { f().await }).await;
        stored.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(step: usize, task: usize, iter: usize) -> TaskKey {
        TaskKey {
            step_idx: step,
            task_idx: task,
            loop_iter: iter,
        }
    }

    #[tokio::test]
    async fn runs_closure_once_for_same_key() {
        let coord = Arc::new(HostCoordinator::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = coord.clone();
            let ca = calls.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_run(key(0, 0, 0), || async move {
                    ca.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Ok(HostOutput {
                        stdout: "shared".into(),
                        stderr: String::new(),
                        exit_code: 0,
                    })
                })
                .await
            }));
        }
        for h in handles {
            let out = h.await.unwrap().expect("ok");
            assert_eq!(out.stdout, "shared");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn broadcasts_error_to_all_callers() {
        let coord = Arc::new(HostCoordinator::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let c = coord.clone();
            let ca = calls.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_run(key(1, 0, 0), || async move {
                    ca.fetch_add(1, Ordering::SeqCst);
                    Err::<HostOutput, String>("boom".into())
                })
                .await
            }));
        }
        for h in handles {
            let err = h.await.unwrap().expect_err("err");
            assert_eq!(err, "boom");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_keys_run_independently() {
        let coord = HostCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));

        for i in 0..3 {
            let ca = calls.clone();
            let _ = coord
                .get_or_run(key(0, i, 0), || async move {
                    ca.fetch_add(1, Ordering::SeqCst);
                    Ok(HostOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    })
                })
                .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
