//! A helper thread that runs one query at a time for callers that must not wait long (plain
//! `std`, unit-tested on every OS).
//!
//! `Platform` methods run on the UI thread and have to return quickly, but a Core Audio query
//! can stall: the HAL may hold a process's requests while macOS shows the audio-capture prompt,
//! which stays up for up to a minute, and a busy machine (a game at full load) can delay the
//! worker. The picker's queries therefore run here. A caller that would wait too long gets
//! `None` and shows its previous answer; the query finishes in the background, no new one is
//! queued behind it, and its late answer is handed to the next call instead of being lost (so
//! queries that are always a little slow still refresh the picker, one call behind).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send>;

/// Where the answers to one kind of query arrive. Keep one per kind and pass it to every
/// [`Worker::run`] of that kind.
pub struct Answer<T>(Arc<(Mutex<Option<T>>, Condvar)>);

impl<T> Default for Answer<T> {
    fn default() -> Self {
        Self(Arc::new((Mutex::new(None), Condvar::new())))
    }
}

impl<T> Clone for Answer<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Answer<T> {
    /// Ends a job: stores its answer (none if it panicked) and frees the worker under the
    /// answer's lock, so a caller woken by the answer already finds the worker idle and can
    /// ask again at once.
    fn finish(&self, value: Option<T>, busy: &AtomicBool) {
        let (slot, arrived) = &*self.0;
        let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if value.is_some() {
            *guard = value;
        }
        busy.store(false, Ordering::Release);
        drop(guard);
        arrived.notify_all();
    }
}

pub struct Worker {
    jobs: Sender<Job>,
    /// A job is queued or running.
    busy: Arc<AtomicBool>,
}

impl Worker {
    /// Starts the thread. It ends when the `Worker` is dropped and its current job returns.
    pub fn new(name: &str) -> std::io::Result<Self> {
        let (jobs, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new().name(name.to_string()).spawn(move || {
            for job in rx {
                job();
            }
        })?;
        Ok(Self { jobs, busy: Arc::new(AtomicBool::new(false)) })
    }

    /// Runs `f` on the worker (unless an earlier query is still running there) and waits up to
    /// `wait` for its answer. Returns that answer, else one that arrived in `answer` after an
    /// earlier call stopped waiting, else `None` (also when `f` panics; the worker survives).
    pub fn run<T: Send + 'static>(
        &self,
        answer: &Answer<T>,
        wait: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (slot, arrived) = &*answer.0;
        // Held until the wait below, so the new job cannot store its answer before the late
        // one is taken out.
        let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        let started = !self.busy.swap(true, Ordering::AcqRel);
        // An answer that came in after an earlier call gave up: the fallback.
        let late = guard.take();
        if !started {
            return late;
        }
        let busy = self.busy.clone();
        let target = answer.clone();
        let job: Job = Box::new(move || {
            let value = catch_unwind(AssertUnwindSafe(f));
            if value.is_err() {
                log::error!("a background audio query panicked");
            }
            target.finish(value.ok(), &busy);
        });
        if self.jobs.send(job).is_err() {
            self.busy.store(false, Ordering::Release);
            return late;
        }
        let (mut guard, _) =
            arrived.wait_timeout_while(guard, wait, |value| value.is_none()).unwrap_or_else(PoisonError::into_inner);
        guard.take().or(late)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const LONG: Duration = Duration::from_secs(10);
    const SHORT: Duration = Duration::from_millis(20);

    #[test]
    fn returns_quick_results() {
        let w = Worker::new("test-worker").unwrap();
        let answer = Answer::default();
        let started = Instant::now();
        assert_eq!(w.run(&answer, LONG, || 6 * 7), Some(42));
        assert_eq!(w.run(&answer, LONG, || 43), Some(43));
        assert!(started.elapsed() < LONG / 2, "an answer ends the wait at once");
    }

    /// Waits until the worker has finished its current job.
    fn until_idle(w: &Worker) {
        let deadline = Instant::now() + LONG;
        while w.busy.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "worker never freed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn slow_queries_time_out_without_queueing_more() {
        let w = Worker::new("test-worker").unwrap();
        let answer = Answer::default();
        let (release, gate) = mpsc::channel::<()>();
        let slow = w.run(&answer, SHORT, move || {
            let _ = gate.recv();
            1
        });
        assert_eq!(slow, None, "timed out");
        assert_eq!(w.run(&answer, LONG, || 2), None, "still busy: not queued");
        release.send(()).unwrap();
        until_idle(&w);
        // The late answer is not lost; a fresh one (here quick) still wins.
        assert_eq!(w.run(&answer, LONG, || 3), Some(3));
        assert_eq!(w.run(&answer, LONG, || 4), Some(4), "nothing stale left");
    }

    #[test]
    fn late_answers_reach_the_next_call() {
        let w = Worker::new("test-worker").unwrap();
        let answer = Answer::default();
        let (release, gate) = mpsc::channel::<()>();
        assert_eq!(
            w.run(&answer, SHORT, move || {
                let _ = gate.recv();
                1
            }),
            None
        );
        release.send(()).unwrap();
        until_idle(&w);
        // The next query is slow as well: the caller gets the earlier, late answer.
        let (release, gate) = mpsc::channel::<()>();
        assert_eq!(
            w.run(&answer, SHORT, move || {
                let _ = gate.recv();
                2
            }),
            Some(1)
        );
        // While that query runs, its caller's successor gets nothing new.
        assert_eq!(w.run(&answer, SHORT, || 3), None);
        release.send(()).unwrap();
        until_idle(&w);
        assert_eq!(w.run(&answer, SHORT, || 4), Some(4));
    }

    #[test]
    fn kinds_of_queries_keep_their_own_answers() {
        let w = Worker::new("test-worker").unwrap();
        let (apps, outputs) = (Answer::default(), Answer::default());
        assert_eq!(w.run(&apps, LONG, || "apps"), Some("apps"));
        assert_eq!(w.run(&outputs, LONG, || "outputs"), Some("outputs"));
    }

    #[test]
    fn survives_a_panicking_query() {
        let w = Worker::new("test-worker").unwrap();
        let answer = Answer::default();
        assert_eq!(w.run(&answer, SHORT, || -> u8 { panic!("query failed") }), None);
        until_idle(&w);
        assert_eq!(w.run(&answer, LONG, || 5), Some(5));
    }
}
