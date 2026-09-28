//! The step barrier behind `mode "sync"`: no host starts a step until every host still
//! running has finished the previous one.
//!
//! Unlike `tokio::sync::Barrier`, the number of participants can shrink. A host that fails,
//! cannot connect, or finishes leaves — through its [`Seat`]'s `Drop`, so every exit path does
//! it — and the hosts still waiting are released if they were only waiting for it.

use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub struct StepBarrier {
    state: Mutex<State>,
    released: Notify,
}

struct State {
    participants: usize,
    arrived: usize,
    /// Bumped each time the barrier releases, so a waiter can tell its own release from the
    /// wake-up of a later one.
    generation: u64,
}

impl State {
    fn release_if_complete(&mut self, released: &Notify) {
        if self.arrived > 0 && self.arrived >= self.participants {
            self.arrived = 0;
            self.generation += 1;
            released.notify_waiters();
        }
    }
}

impl StepBarrier {
    pub fn new(participants: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                participants,
                arrived: 0,
                generation: 0,
            }),
            released: Notify::new(),
        })
    }

    /// One seat per participant.
    pub fn seat(self: &Arc<Self>) -> Seat {
        Seat {
            barrier: self.clone(),
            status: Mutex::new(SeatStatus::Seated),
        }
    }
}

enum SeatStatus {
    Seated,
    /// Arrived and waiting for this generation's release.
    Waiting(u64),
    Left,
}

/// A host's place at the barrier.
pub struct Seat {
    barrier: Arc<StepBarrier>,
    status: Mutex<SeatStatus>,
}

impl Seat {
    /// Wait until every host still seated has arrived.
    pub async fn arrive(&self) {
        let barrier = &self.barrier;
        let generation = {
            let mut status = self.status.lock().unwrap();
            let mut state = barrier.state.lock().unwrap();
            let generation = state.generation;
            // A seat whose earlier wait was abandoned is still counted for this step; it
            // resumes that wait rather than arriving twice, which would release the others
            // before every host had finished.
            let resuming = matches!(*status, SeatStatus::Waiting(g) if g == generation);
            if !resuming {
                state.arrived += 1;
                state.release_if_complete(&barrier.released);
                if state.generation != generation {
                    *status = SeatStatus::Seated;
                    return;
                }
            }
            *status = SeatStatus::Waiting(generation);
            generation
        };

        loop {
            let released = barrier.released.notified();
            tokio::pin!(released);
            // Registered before the generation is re-checked, so a release in between is not
            // missed.
            released.as_mut().enable();
            if barrier.state.lock().unwrap().generation != generation {
                break;
            }
            released.await;
        }
        *self.status.lock().unwrap() = SeatStatus::Seated;
    }

    /// Stop taking part. Idempotent; also done on drop.
    pub fn leave(&self) {
        let mut status = self.status.lock().unwrap();
        if matches!(*status, SeatStatus::Left) {
            return;
        }
        let mut state = self.barrier.state.lock().unwrap();
        // A wait abandoned before its release still counts as arrived; withdraw it, or the
        // others could be released while a host that is still running has not finished.
        if let SeatStatus::Waiting(generation) = *status {
            if state.generation == generation {
                state.arrived -= 1;
            }
        }
        state.participants -= 1;
        state.release_if_complete(&self.barrier.released);
        *status = SeatStatus::Left;
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    const SHORT: Duration = Duration::from_millis(100);
    const LONG: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_single_host_never_waits() {
        let seat = StepBarrier::new(1).seat();
        timeout(LONG, seat.arrive()).await.unwrap();
        timeout(LONG, seat.arrive()).await.unwrap();
    }

    #[tokio::test]
    async fn hosts_are_held_until_all_arrive() {
        let barrier = StepBarrier::new(2);
        let (a, b) = (barrier.seat(), barrier.seat());
        assert!(
            timeout(SHORT, a.arrive()).await.is_err(),
            "the first host must wait for the second"
        );
        // `a`'s wait was abandoned above; it arrives again, then `b` completes the step.
        let a = tokio::spawn(async move {
            a.arrive().await;
            a
        });
        tokio::time::sleep(SHORT).await;
        timeout(LONG, b.arrive()).await.unwrap();
        timeout(LONG, a).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn successive_steps_each_wait_for_everyone() {
        let barrier = StepBarrier::new(3);
        let seats: Vec<Seat> = (0..3).map(|_| barrier.seat()).collect();
        let handles: Vec<_> = seats
            .into_iter()
            .map(|seat| {
                tokio::spawn(async move {
                    for _ in 0..5 {
                        seat.arrive().await;
                    }
                })
            })
            .collect();
        for h in handles {
            timeout(LONG, h).await.unwrap().unwrap();
        }
    }

    /// A failed host must not hold the others forever.
    #[tokio::test]
    async fn a_host_that_leaves_releases_the_others() {
        let barrier = StepBarrier::new(2);
        let (a, b) = (barrier.seat(), barrier.seat());
        let waiting = tokio::spawn(async move { a.arrive().await });
        tokio::time::sleep(SHORT).await;
        drop(b);
        timeout(LONG, waiting).await.unwrap().unwrap();
    }

    /// An abandoned wait must be withdrawn, not left counted as an arrival that could
    /// release the others early.
    #[tokio::test]
    async fn an_abandoned_wait_does_not_count_after_leaving() {
        let barrier = StepBarrier::new(3);
        let (a, b, c) = (barrier.seat(), barrier.seat(), barrier.seat());
        let a_waiting = tokio::spawn(async move { a.arrive().await });
        tokio::time::sleep(SHORT).await;

        assert!(timeout(SHORT, b.arrive()).await.is_err());
        drop(b);
        tokio::time::sleep(SHORT).await;
        assert!(
            !a_waiting.is_finished(),
            "`c` has not arrived, so `a` must still be waiting"
        );

        timeout(LONG, c.arrive()).await.unwrap();
        timeout(LONG, a_waiting).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn leaving_twice_counts_once() {
        let barrier = StepBarrier::new(3);
        let (a, b, c) = (barrier.seat(), barrier.seat(), barrier.seat());
        b.leave();
        b.leave();
        drop(b);
        let a_waiting = tokio::spawn(async move { a.arrive().await });
        tokio::time::sleep(SHORT).await;
        assert!(!a_waiting.is_finished(), "`c` is still seated");
        timeout(LONG, c.arrive()).await.unwrap();
        timeout(LONG, a_waiting).await.unwrap().unwrap();
    }
}
