//! COM and Win32 plumbing shared by the Windows backend: MTA apartments and worker threads,
//! owned handles and events, CoTaskMem strings, MMCSS and the OS build number.
//!
//! winit owns the UI thread as a COM STA, so nothing here ever initializes COM on a caller's
//! thread: every COM call runs on a thread this backend created and joined to the MTA.

use std::any::Any;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, STILL_ACTIVE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, CreateMutexW, GetExitCodeProcess,
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    ReleaseMutex, SetEvent, WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR, w};

/// Membership of the calling thread in the process-wide multithreaded apartment, for as long as
/// the value lives. Not `Send`: COM initialization is per thread.
pub struct Mta(PhantomData<*const ()>);

impl Mta {
    pub fn enter() -> anyhow::Result<Self> {
        // S_FALSE (already in the MTA) is a success and still needs a matching CoUninitialize.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("joining the COM multithreaded apartment")?;
        Ok(Mta(PhantomData))
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// A kernel handle closed on drop.
#[derive(Debug)]
pub struct Handle(HANDLE);

// Kernel handles are process-wide values; closing and waiting are thread-safe.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// # Safety
    /// `h` must be a valid, owned handle that nothing else closes.
    pub unsafe fn from_raw(h: HANDLE) -> Self {
        Handle(h)
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// An event object; `manual_reset` events stay signalled until reset.
    pub fn event(manual_reset: bool) -> anyhow::Result<Self> {
        let h = unsafe { CreateEventW(None, manual_reset, false, None) }.context("CreateEventW")?;
        Ok(Handle(h))
    }

    pub fn set(&self) {
        // Only fails for an invalid handle, which `Handle` rules out.
        let _ = unsafe { SetEvent(self.0) };
    }

    /// True if the object is signalled (an event is set, a process has exited).
    pub fn is_signalled(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_OBJECT_0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// A mutex shared by every process of the user's logon session, for state that several
/// processes read and rewrite (the hush journal: Sidestage instances and their guardians).
pub struct NamedMutex(Handle);

/// Ownership of a [`NamedMutex`]; released on drop. Not `Send`: a mutex belongs to the thread
/// that acquired it.
pub struct NamedMutexGuard<'a>(&'a NamedMutex, PhantomData<*const ()>);

impl NamedMutex {
    /// `key` identifies the protected state (e.g. a file path); it is hashed into the name.
    pub fn new(key: &str) -> anyhow::Result<Self> {
        // FNV-1a: object names cannot contain backslashes, and a hash keeps them short.
        let hash = key
            .to_lowercase()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
        let name = HSTRING::from(format!("Local\\Sidestage-{hash:016x}"));
        let h = unsafe { CreateMutexW(None, false, &name) }.context("CreateMutexW")?;
        Ok(NamedMutex(Handle(h)))
    }

    /// Waits up to `timeout` for the mutex. A mutex left behind by a process that died while
    /// holding it is acquired too (the state it protects is rewritten atomically anyway).
    pub fn lock(&self, timeout: Duration) -> Option<NamedMutexGuard<'_>> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let w = unsafe { WaitForSingleObject(self.0.raw(), ms) };
        (w == WAIT_OBJECT_0 || w == WAIT_ABANDONED).then_some(NamedMutexGuard(self, PhantomData))
    }
}

impl Drop for NamedMutexGuard<'_> {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0.0.raw()) };
    }
}

/// Opens a process with the given rights.
pub fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Handle> {
    if pid == 0 {
        return None;
    }
    unsafe { OpenProcess(access, false, pid) }.ok().map(Handle)
}

/// Creation time of a process (FILETIME as u64), the PID-reuse guard used throughout the backend.
pub fn process_created(pid: u32) -> Option<u64> {
    let h = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    created_of(&h)
}

pub fn created_of(h: &Handle) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(h.raw(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// True if the process with this PID is running and is the same process that was created at
/// `created` (0 = do not check).
pub fn process_alive(pid: u32, created: u64) -> bool {
    let running = if let Some(h) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        (unsafe { WaitForSingleObject(h.raw(), 0) } == WAIT_TIMEOUT).then_some(h)
    } else {
        // Elevated or protected processes may refuse SYNCHRONIZE but still allow limited
        // queries; the exit code then tells (STILL_ACTIVE while running).
        open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).filter(|h| {
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(h.raw(), &mut code) }.is_ok() && code == STILL_ACTIVE.0 as u32
        })
    };
    running.is_some_and(|h| created == 0 || created_of(&h) == Some(created))
}

/// Copies a CoTaskMem-allocated wide string and frees it.
pub fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// Raises the calling thread to the MMCSS "Pro Audio" class (reverted on drop). Failure is not
/// fatal: the thread just runs at normal priority.
pub struct Mmcss(Option<HANDLE>);

impl Mmcss {
    pub fn pro_audio() -> Self {
        let mut index = 0u32;
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) } {
            Ok(h) => Mmcss(Some(h)),
            Err(e) => {
                log::warn!("MMCSS 'Pro Audio' unavailable, audio thread runs at normal priority: {e}");
                Mmcss(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// The Windows build number (e.g. 26100), read with `RtlGetVersion` because `GetVersionEx`
/// lies to unmanifested processes. 0 if it cannot be read.
pub fn os_build() -> u32 {
    let mut v = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    let status = unsafe { RtlGetVersion(&mut v) };
    if status.0 >= 0 { v.dwBuildNumber } else { 0 }
}

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

enum Msg<S> {
    Run(Job<S>),
    Stop,
}

/// A thread in the MTA that owns some COM state `S` (COM pointers are not `Send`, so the state
/// is created on the thread and never leaves it) and runs jobs against it. An optional `tick`
/// runs every `period` between jobs.
pub struct Worker<S: 'static> {
    tx: mpsc::Sender<Msg<S>>,
    thread: Option<JoinHandle<()>>,
}

/// Queues jobs on a [`Worker`] without waiting for them, e.g. from COM notification sinks.
/// Unlike a channel sender it does not keep the worker alive, so the worker's own state may
/// hold posters (its sinks are released when the state is dropped at shutdown).
pub struct Poster<S: 'static>(mpsc::Sender<Msg<S>>);

impl<S: 'static> Clone for Poster<S> {
    fn clone(&self) -> Self {
        Poster(self.0.clone())
    }
}

impl<S: 'static> Poster<S> {
    /// Queues `f`; silently dropped once the worker has stopped.
    pub fn post(&self, f: impl FnOnce(&mut S) + Send + 'static) {
        let _ = self.0.send(Msg::Run(Box::new(f)));
    }
}

impl<S: 'static> Worker<S> {
    pub fn spawn(
        name: &str,
        period: Duration,
        init: impl FnOnce() -> anyhow::Result<S> + Send + 'static,
        mut tick: impl FnMut(&mut S) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg<S>>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mta = match Mta::enter() {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut state = match init() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let mut next_tick = Instant::now() + period;
                loop {
                    let wait = next_tick.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(Msg::Run(job)) => {
                            if let Err(p) = catch_unwind(AssertUnwindSafe(|| job(&mut state))) {
                                log::error!("audio worker job panicked: {}", panic_message(&p));
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if Instant::now() >= next_tick {
                        if let Err(p) = catch_unwind(AssertUnwindSafe(|| tick(&mut state))) {
                            log::error!("audio worker tick panicked: {}", panic_message(&p));
                        }
                        next_tick = Instant::now() + period;
                    }
                }
                drop(state);
                drop(mta);
            })
            .context("spawning the audio worker thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Worker { tx, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the audio worker thread failed to start"))
            }
        }
    }

    /// Runs `f` on the worker and waits up to `timeout` for its result.
    pub fn call<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> anyhow::Result<R> {
        let (tx, rx) = mpsc::sync_channel(1);
        let job: Job<S> = Box::new(move |s| {
            let _ = tx.send(f(s));
        });
        self.tx.send(Msg::Run(job)).map_err(|_| anyhow!("the audio worker has stopped"))?;
        rx.recv_timeout(timeout).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => {
                anyhow!("the Windows audio system did not answer in time")
            }
            mpsc::RecvTimeoutError::Disconnected => {
                anyhow!("the audio worker failed while handling a request")
            }
        })
    }
}

impl<S: 'static> Worker<S> {
    pub fn poster(&self) -> Poster<S> {
        Poster(self.tx.clone())
    }
}

impl<S: 'static> Drop for Worker<S> {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn panic_message(p: &Box<dyn Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_process_identity() {
        assert!(os_build() >= 10_000);
        let me = std::process::id();
        let created = process_created(me).unwrap();
        assert!(process_alive(me, created));
        assert!(!process_alive(me, created + 1), "a different creation time is another process");
        assert!(!process_alive(0, 0));
    }

    #[test]
    fn worker_runs_jobs_in_the_mta() {
        let w = Worker::spawn("test-worker", Duration::from_millis(10), || Ok(0u32), |n| *n += 1).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let ticks = w.call(Duration::from_secs(1), |n| *n).unwrap();
        assert!(ticks > 0, "the tick runs between jobs");
        // The worker is in the MTA: joining it again succeeds (an STA thread would fail with
        // RPC_E_CHANGED_MODE).
        let mta = w.call(Duration::from_secs(1), |_| Mta::enter().is_ok()).unwrap();
        assert!(mta);
    }

    #[test]
    fn posters_run_jobs_without_keeping_the_worker_alive() {
        struct St {
            n: u32,
            poster: Option<Poster<St>>,
        }
        let w =
            Worker::spawn("test-poster", Duration::from_secs(10), || Ok(St { n: 0, poster: None }), |_| {}).unwrap();
        let p = w.poster();
        w.call(Duration::from_secs(1), move |s| s.poster = Some(p)).unwrap();
        w.poster().post(|s| s.n += 1);
        let n = w.call(Duration::from_secs(1), |s| s.n).unwrap();
        assert_eq!(n, 1, "posted jobs run in order before later calls");
        // The state holds a poster; dropping the worker must still stop and join it.
        drop(w);
    }
}
