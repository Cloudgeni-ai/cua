//! Reading a window after an action once what the action set off is over.
//!
//! An action returns when the app has taken it, not when the app has shown
//! its result. A press that opens another screen starts an animation, and
//! the screen's accessibility tree changes over the next several hundred
//! milliseconds (an iOS Simulator push: changes for about 750 ms after the
//! press on an idle Mac, longer on a busy one). A read right after the
//! action shows the screen before it, or one half way.
//!
//! Apps say when their accessibility tree changes: they post notifications
//! (layout changed, element created, focus moved, a sheet or menu opened).
//! Every action on a process starts watching that process's notifications,
//! and the first `get_window_state` of that process after the action waits
//! only if the window it reads is changing: when the app posts nothing there
//! within [`Timing::first_change`] of the action's end (the action changed
//! nothing it reports, or the app reports nothing), the read goes ahead at
//! once. Otherwise it waits until the window's notifications have stopped
//! for [`Timing::quiet`], and a window that keeps changing is read
//! [`Timing::max_wait`] after the action at the latest. Reads inside a
//! `run_steps` batch never wait: its steps have their own waits and checks.
//!
//! Only notifications that change what the tree holds count. Value and title
//! changes are not watched: a value is read as it is whenever the read comes,
//! and the windows that never stop changing (a clock, a progress bar, a
//! terminal printing, a monitor's numbers) change only values and titles, so
//! an action in them does not wait for that.
//!
//! An app can pause between notifications in the middle of a change (an
//! iOS Simulator push on a busy Mac: one notification as the press lands,
//! up to half a second of none, then the animation's). A read that the
//! window's notifications show changed while it ran may show it half way,
//! so it waits for quiet again and reads again, at most
//! [`Timing::max_reads`] times in all and not past [`Timing::max_wait`].
//!
//! Only the read window's notifications count, and those the app posts for
//! itself rather than for one window. One process can show many windows that
//! change on their own: the iOS Simulator shows every booted device in one
//! process, and another device's screen changing must not hold this read.

use std::collections::{HashMap, VecDeque};
use std::os::raw::c_void;
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult, CFRunLoopSource,
};
use core_foundation::string::{CFString, CFStringRef};
use cua_driver_core::tool_activity::ActionPhase;

use super::bindings::{
    ax_get_window_id, copy_element_attr, kAXErrorSuccess, AXError, AXUIElementCreateApplication,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};

/// How long a read after an action waits (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// How long after the action's end the window's first change may take
    /// to come; with none by then the read goes ahead.
    pub first_change: Duration,
    /// How long the window's changes must have stopped before the read.
    pub quiet: Duration,
    /// The longest a read waits after the action ended.
    pub max_wait: Duration,
    /// The most times one read after an action walks the window.
    pub max_reads: u32,
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        first_change: Duration::from_millis(100),
        quiet: Duration::from_millis(200),
        max_wait: Duration::from_millis(1_000),
        max_reads: 3,
    };
}

static TIMING: OnceLock<Timing> = OnceLock::new();

/// Use `timing` instead of [`Timing::DEFAULT`] in this process; for programs
/// that measure the wait. Returns false once a read has used the timing.
#[doc(hidden)]
pub fn set_timing(timing: Timing) -> bool {
    TIMING.set(timing).is_ok()
}

fn timing() -> Timing {
    *TIMING.get_or_init(|| Timing::DEFAULT)
}

/// A read during an action on the same process waits at most this long for
/// it to end.
const RUNNING_CAP: Duration = Duration::from_secs(10);
/// Processes watched at once; the least recently acted on is dropped.
const MAX_WATCHED: usize = 16;
/// A process whose notifications could not be watched is retried after this.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How often the waiting read checks the notifications.
const POLL: Duration = Duration::from_millis(15);

/// The notifications that mean the tree's content changed. A destroyed
/// element no longer says which window it was in, so element destruction is
/// not among them; the layout change that goes with it is. Value and title
/// changes are left out (see the module documentation).
const NOTIFICATIONS: [&str; 9] = [
    "AXLayoutChanged",
    "AXCreated",
    "AXFocusedUIElementChanged",
    "AXSelectedChildrenChanged",
    "AXRowCountChanged",
    "AXWindowCreated",
    "AXSheetCreated",
    "AXMenuOpened",
    "AXLoadComplete",
];

/// The changes the app reported for one window, or for itself.
#[derive(Clone, Copy, Debug, Default)]
struct Changes {
    latest: Option<Instant>,
    count: u64,
}

impl Changes {
    fn note(&mut self, at: Instant) {
        self.latest = Some(self.latest.map_or(at, |latest| latest.max(at)));
        self.count += 1;
    }
}

/// What the first read after an action waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// The app posted no change within [`Timing::first_change`].
    NoChange,
    /// The app's changes stopped for [`Timing::quiet`].
    Settled,
    /// The app was still changing [`Timing::max_wait`] after the action.
    StillChanging,
}

impl Settle {
    pub fn as_str(self) -> &'static str {
        match self {
            Settle::NoChange => "no_change",
            Settle::Settled => "settled",
            Settle::StillChanging => "still_changing",
        }
    }
}

/// One action whose result no read has waited for yet.
#[derive(Clone, Copy, Debug)]
struct Pending {
    started: Instant,
    /// `None` while the action runs.
    ended: Option<Instant>,
}

#[derive(Debug, Default)]
struct Process {
    /// Notifications by the window of the element they name.
    windows: HashMap<u32, Changes>,
    /// Notifications that name no window: the application itself.
    app: Changes,
    pending: Option<Pending>,
    last_action: Option<Instant>,
}

#[derive(Default)]
struct State {
    processes: HashMap<i32, Process>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static WATCHER: OnceLock<Mutex<mpsc::Sender<i32>>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Register [`note_action`] as the registry's action hook. Safe to call more
/// than once.
pub fn install() {
    cua_driver_core::tool_activity::set_action_activity_fn(note_action);
}

/// An action on `pid` starts or ends. Starting one watches the process's
/// notifications (once per process) and marks its next read to wait.
pub fn note_action(pid: i64, phase: ActionPhase) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 0 {
        return;
    }
    let now = Instant::now();
    let mut state = state();
    let process = state.processes.entry(pid).or_default();
    match phase {
        ActionPhase::Started => {
            process.last_action = Some(now);
            // A batch's steps keep the first step's start, so a change one
            // of them set off counts.
            let started = process
                .pending
                .filter(|pending| {
                    pending
                        .ended
                        .is_none_or(|ended| now - ended < timing().max_wait)
                })
                .map_or(now, |pending| pending.started);
            if started == now {
                // Counts start with the action.
                process.windows.clear();
                process.app = Changes::default();
            }
            process.pending = Some(Pending {
                started,
                ended: None,
            });
        }
        ActionPhase::Finished => {
            if let Some(pending) = process.pending.as_mut() {
                pending.ended = Some(now);
            }
        }
    }
    drop(state);
    if phase == ActionPhase::Started {
        watch(pid);
    }
}

/// What to do at `now` for a read after an action, from when it started and
/// ended and the app's latest notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Read(Settle),
    Wait,
}

fn decide(
    timing: Timing,
    now: Instant,
    pending: Pending,
    last_change: Option<Instant>,
) -> Decision {
    if pending.ended.is_none() && now >= pending.started + RUNNING_CAP {
        return Decision::Read(Settle::StillChanging);
    }
    let ended = pending.ended.unwrap_or(now);
    let change = last_change.filter(|change| *change >= pending.started);
    // Every read gives a change the action set off `first_change` to show
    // up; quiet counts from the last change, so changes made while the
    // action ran cost nothing more.
    let first_change_over = now >= ended + timing.first_change;
    match change {
        None if first_change_over => Decision::Read(Settle::NoChange),
        Some(change) if first_change_over && now >= change + timing.quiet => {
            Decision::Read(Settle::Settled)
        }
        _ if now >= ended + timing.max_wait => Decision::Read(Settle::StillChanging),
        _ => Decision::Wait,
    }
}

impl Process {
    /// The changes a read of `window_id` waits for: that window's and the
    /// application's.
    fn changes_for(&self, window_id: u32) -> Changes {
        let window = self.windows.get(&window_id).copied().unwrap_or_default();
        Changes {
            latest: window.latest.max(self.app.latest),
            count: window.count + self.app.count,
        }
    }

    fn note_change(&mut self, window_id: Option<u32>, at: Instant) {
        match window_id {
            Some(window_id) => {
                // Windows that stopped changing long ago are forgotten.
                if self.windows.len() >= 64 {
                    self.windows.retain(|_, changes| {
                        changes
                            .latest
                            .is_some_and(|latest| at - latest < RETRY_AFTER)
                    });
                }
                self.windows.entry(window_id).or_default().note(at);
            }
            None => self.app.note(at),
        }
    }
}

/// How the first read of a window after an action waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleReport {
    pub settle: Settle,
    /// Time spent waiting, not reading.
    pub waited: Duration,
    /// Notifications for the window (or the app) since the action started.
    pub changes: u64,
    /// Walks of the window, counting the one the read returns.
    pub reads: u32,
}

/// The first read of a window after an action on its process. Holds the
/// action's wait open while the read runs, so a change during the read is
/// seen; dropping it ends the wait, and the next read goes ahead at once.
#[derive(Debug)]
pub struct SettledRead {
    pid: i32,
    window_id: u32,
    started: Instant,
    timing: Timing,
    report: SettleReport,
}

/// Before a read of `pid`'s window `window_id`: if an action on the process
/// has not been waited for yet, wait until what it set off in that window is
/// over (see the module documentation). `None` when there is nothing to wait
/// for.
pub async fn settle_before_read(pid: i32, window_id: u32) -> Option<SettledRead> {
    let started = {
        let state = state();
        state.processes.get(&pid)?.pending?.started
    };
    let mut read = SettledRead {
        pid,
        window_id,
        started,
        timing: timing(),
        report: SettleReport {
            settle: Settle::NoChange,
            waited: Duration::ZERO,
            changes: 0,
            reads: 0,
        },
    };
    read.wait().await;
    Some(read)
}

impl SettledRead {
    pub fn report(&self) -> SettleReport {
        self.report
    }

    /// Whether a walk that started at `walk_started` should be done again:
    /// the window changed after it started, a read may still follow
    /// ([`Timing::max_reads`]), and the action ended less than [`Timing::max_wait`] ago. If
    /// so, waits for the change to be over first.
    pub async fn read_again(&mut self, walk_started: Instant) -> bool {
        if self.report.reads >= self.timing.max_reads {
            return false;
        }
        let changed = {
            let state = state();
            let Some(process) = state.processes.get(&self.pid) else {
                return false;
            };
            let Some(pending) = process
                .pending
                .filter(|pending| pending.started == self.started)
            else {
                return false;
            };
            let in_time = pending
                .ended
                .is_none_or(|ended| Instant::now() < ended + self.timing.max_wait);
            in_time
                && process
                    .changes_for(self.window_id)
                    .latest
                    .is_some_and(|latest| latest >= walk_started)
        };
        if !changed {
            return false;
        }
        self.wait().await;
        true
    }

    /// Wait as [`decide`] says, and count the walk that follows.
    async fn wait(&mut self) {
        let begun = Instant::now();
        loop {
            let decision = {
                let state = state();
                let Some(process) = state.processes.get(&self.pid) else {
                    break;
                };
                let Some(pending) = process
                    .pending
                    .filter(|pending| pending.started == self.started)
                else {
                    break;
                };
                let changes = process.changes_for(self.window_id);
                self.report.changes = changes.count;
                decide(self.timing, Instant::now(), pending, changes.latest)
            };
            if let Decision::Read(settle) = decision {
                self.report.settle = settle;
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        self.report.waited += begun.elapsed();
        self.report.reads += 1;
    }
}

impl Drop for SettledRead {
    fn drop(&mut self) {
        let mut state = state();
        if let Some(process) = state.processes.get_mut(&self.pid) {
            if process
                .pending
                .is_some_and(|pending| pending.started == self.started)
            {
                process.pending = None;
            }
        }
    }
}

// ── Notification watching ───────────────────────────────────────────────────

type AXObserverRef = *mut c_void;
type ObserverCallback =
    unsafe extern "C" fn(AXObserverRef, AXUIElementRef, CFStringRef, *mut c_void);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXObserverCreate(
        pid: i32,
        callback: ObserverCallback,
        observer: *mut AXObserverRef,
    ) -> AXError;
    fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
    fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> *mut c_void;
}

unsafe extern "C" fn on_change(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    _notification: CFStringRef,
    refcon: *mut c_void,
) {
    let pid = refcon as usize as i32;
    let now = Instant::now();
    // Only a read after an action looks at the changes, so the window is
    // looked up (a request to the app) only while one may.
    let waited_for = state()
        .processes
        .get(&pid)
        .is_some_and(|process| process.pending.is_some());
    if !waited_for || element.is_null() {
        return;
    }
    let window_id = window_of(element);
    if let Some(process) = state().processes.get_mut(&pid) {
        process.note_change(window_id, now);
    }
}

/// The CGWindowID of the window `element` is in, or `None` for an element
/// in no window (the application itself) or one that no longer answers.
unsafe fn window_of(element: AXUIElementRef) -> Option<u32> {
    // A slow app must not hold the watcher long.
    AXUIElementSetMessagingTimeout(element, 0.25);
    if let Some(window_id) = ax_get_window_id(element) {
        return Some(window_id);
    }
    let window = copy_element_attr(element, "AXWindow")?;
    let window_id = ax_get_window_id(window);
    CFRelease(window as CFTypeRef);
    window_id
}

/// Ask the watcher thread to watch `pid`. Never blocks the action.
fn watch(pid: i32) {
    let sender = WATCHER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cua-ax-activity".into())
            .spawn(move || watcher(receiver));
        if let Err(error) = spawned {
            tracing::warn!("accessibility activity watcher did not start: {error}");
        }
        Mutex::new(sender)
    });
    let _ = sender
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(pid);
}

struct Observer {
    observer: AXObserverRef,
    source: CFRunLoopSource,
}

impl Drop for Observer {
    fn drop(&mut self) {
        CFRunLoop::get_current().remove_source(&self.source, unsafe { kCFRunLoopDefaultMode });
        // SAFETY: created by AXObserverCreate and owned here.
        unsafe { CFRelease(self.observer as CFTypeRef) };
    }
}

/// The watcher thread: owns every observer and runs the run loop their
/// notifications are delivered on.
fn watcher(requests: mpsc::Receiver<i32>) {
    // Most recently acted on last.
    let mut watched: VecDeque<(i32, Option<Observer>, Instant)> = VecDeque::new();
    loop {
        loop {
            match requests.try_recv() {
                Ok(pid) => {
                    let now = Instant::now();
                    let position = watched
                        .iter()
                        .position(|(watched_pid, _, _)| *watched_pid == pid);
                    let entry = match position.and_then(|index| watched.remove(index)) {
                        Some((_, None, tried)) if now - tried >= RETRY_AFTER => {
                            (pid, unsafe { observe(pid) }, now)
                        }
                        Some(entry) => entry,
                        None => (pid, unsafe { observe(pid) }, now),
                    };
                    watched.push_back(entry);
                    while watched.len() > MAX_WATCHED {
                        if let Some((dropped, _, _)) = watched.pop_front() {
                            state().processes.remove(&dropped);
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let ran = CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(20),
            true,
        );
        if ran == CFRunLoopRunResult::Finished {
            // No observer yet: nothing for the run loop to wait on.
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Watch `pid`'s notifications on this thread's run loop. `None` when the
/// app cannot be observed (not trusted, gone, or it refuses every
/// notification).
unsafe fn observe(pid: i32) -> Option<Observer> {
    let mut observer: AXObserverRef = std::ptr::null_mut();
    if AXObserverCreate(pid, on_change, &mut observer) != kAXErrorSuccess || observer.is_null() {
        return None;
    }
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        CFRelease(observer as CFTypeRef);
        return None;
    }
    // A slow app must not hold the watcher long: registration is a request
    // per notification.
    super::bindings::AXUIElementSetMessagingTimeout(application, 0.5);
    let refcon = pid as usize as *mut c_void;
    let registered = NOTIFICATIONS
        .iter()
        .filter(|name| {
            let name = CFString::new(name);
            AXObserverAddNotification(observer, application, name.as_concrete_TypeRef(), refcon)
                == kAXErrorSuccess
        })
        .count();
    CFRelease(application as CFTypeRef);
    if registered == 0 {
        CFRelease(observer as CFTypeRef);
        return None;
    }
    let source = CFRunLoopSource::wrap_under_get_rule(AXObserverGetRunLoopSource(observer) as _);
    CFRunLoop::get_current().add_source(&source, kCFRunLoopDefaultMode);
    Some(Observer { observer, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    const T: Timing = Timing {
        first_change: Duration::from_millis(100),
        quiet: Duration::from_millis(200),
        max_wait: Duration::from_millis(1_000),
        max_reads: 3,
    };

    #[test]
    fn a_read_waits_until_the_changes_stop() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: Some(start + ms(200)),
        };
        // Changes from the press through an animation.
        let last = Some(start + ms(700));
        assert_eq!(decide(T, start + ms(750), pending, last), Decision::Wait);
        assert_eq!(
            decide(T, start + ms(700) + T.quiet, pending, last),
            Decision::Read(Settle::Settled)
        );
    }

    #[test]
    fn changes_made_while_the_action_ran_cost_only_the_first_change_wait() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: Some(start + ms(400)),
        };
        // Changes while the action ran, then nothing.
        let last = Some(start + ms(100));
        assert_eq!(decide(T, start + ms(450), pending, last), Decision::Wait);
        assert_eq!(
            decide(T, start + ms(400) + T.first_change, pending, last),
            Decision::Read(Settle::Settled)
        );
        // One just before the action ended still needs its quiet.
        let late = Some(start + ms(390));
        assert_eq!(
            decide(T, start + ms(400) + T.first_change, pending, late),
            Decision::Wait
        );
        assert_eq!(
            decide(T, start + ms(390) + T.quiet, pending, late),
            Decision::Read(Settle::Settled)
        );
    }

    #[test]
    fn no_change_reads_after_the_grace() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: Some(start),
        };
        // A change before the action is not the action's.
        let earlier = start.checked_sub(ms(50));
        assert_eq!(decide(T, start + ms(50), pending, earlier), Decision::Wait);
        assert_eq!(
            decide(T, start + ms(100), pending, earlier),
            Decision::Read(Settle::NoChange)
        );
    }

    #[test]
    fn an_app_that_keeps_changing_is_read_at_the_cap() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: Some(start),
        };
        assert_eq!(
            decide(T, start + ms(990), pending, Some(start + ms(980))),
            Decision::Wait
        );
        assert_eq!(
            decide(T, start + ms(1_000), pending, Some(start + ms(990))),
            Decision::Read(Settle::StillChanging)
        );
    }

    #[test]
    fn a_running_action_is_waited_for() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: None,
        };
        assert_eq!(decide(T, start + ms(5_000), pending, None), Decision::Wait);
        assert_eq!(
            decide(T, start + RUNNING_CAP, pending, None),
            Decision::Read(Settle::StillChanging)
        );
    }

    #[test]
    fn a_read_counts_its_window_and_the_application_only() {
        let start = Instant::now();
        let mut process = Process::default();
        process.note_change(Some(7), start + ms(100));
        process.note_change(Some(8), start + ms(900));
        assert_eq!(process.changes_for(7).latest, Some(start + ms(100)));
        assert_eq!(process.changes_for(7).count, 1);
        process.note_change(None, start + ms(400));
        assert_eq!(process.changes_for(7).latest, Some(start + ms(400)));
        assert_eq!(process.changes_for(7).count, 2);
        assert_eq!(process.changes_for(8).latest, Some(start + ms(900)));
        assert_eq!(process.changes_for(9).latest, Some(start + ms(400)));
    }

    #[test]
    fn another_window_changing_does_not_hold_the_read() {
        let start = Instant::now();
        let pending = Pending {
            started: start,
            ended: Some(start + ms(200)),
        };
        let mut process = Process::default();
        process.note_change(Some(7), start + ms(300));
        // Another device's screen keeps animating.
        for at in (0..2_000).step_by(50) {
            process.note_change(Some(8), start + ms(at));
        }
        assert_eq!(
            decide(T, start + ms(600), pending, process.changes_for(7).latest),
            Decision::Read(Settle::Settled)
        );
        assert_eq!(
            decide(T, start + ms(600), pending, process.changes_for(8).latest),
            Decision::Wait
        );
    }

    #[tokio::test]
    async fn only_a_read_after_an_action_waits() {
        let pid = 987_654;
        assert!(settle_before_read(pid, 1).await.is_none());
        {
            let mut state = state();
            let process = state.processes.entry(pid).or_default();
            let now = Instant::now();
            process.pending = Some(Pending {
                started: now,
                ended: Some(now),
            });
        }
        let read = settle_before_read(pid, 1).await.unwrap();
        let report = read.report();
        assert_eq!(report.settle, Settle::NoChange);
        assert_eq!(report.reads, 1);
        assert!(report.waited >= Timing::DEFAULT.first_change - ms(20));
        drop(read);
        assert!(
            settle_before_read(pid, 1).await.is_none(),
            "the next read does not wait"
        );
    }

    #[tokio::test]
    async fn a_window_that_changed_during_the_read_is_read_again() {
        let pid = 987_655;
        let now = Instant::now();
        state().processes.entry(pid).or_default().pending = Some(Pending {
            started: now,
            ended: Some(now),
        });
        let mut read = settle_before_read(pid, 3).await.unwrap();
        let walk_started = Instant::now();
        assert!(!read.read_again(walk_started).await, "nothing changed");
        // The window changes while it is read; another window's change does
        // not count.
        state()
            .processes
            .get_mut(&pid)
            .unwrap()
            .note_change(Some(4), Instant::now());
        assert!(!read.read_again(walk_started).await);
        state()
            .processes
            .get_mut(&pid)
            .unwrap()
            .note_change(Some(3), Instant::now());
        let before = Instant::now();
        assert!(read.read_again(walk_started).await);
        assert!(
            before.elapsed() >= Timing::DEFAULT.quiet - ms(20),
            "waited for quiet again"
        );
        assert_eq!(read.report().reads, 2);
        assert_eq!(read.report().settle, Settle::Settled);
        assert_eq!(read.report().changes, 1);
    }
}
