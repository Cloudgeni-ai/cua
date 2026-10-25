//! Per-call activity hook: the platform shell learns that an agent is
//! working each time the registry dispatches a tool.
//!
//! macOS uses it to keep the display awake while an agent works, because the
//! screen saver and display sleep put the session behind the lock screen,
//! where no app's windows are reachable. Registered once by the platform
//! crate; a no-op when nothing is registered.

use std::sync::OnceLock;

type ToolActivityFn = Box<dyn Fn() + Send + Sync>;
static TOOL_ACTIVITY_FN: OnceLock<ToolActivityFn> = OnceLock::new();

/// Register the activity callback. Returns false when one is already set;
/// the first registration wins.
pub fn set_tool_activity_fn(f: impl Fn() + Send + Sync + 'static) -> bool {
    TOOL_ACTIVITY_FN.set(Box::new(f)).is_ok()
}

/// Report one tool dispatch. Must stay cheap: it runs on every call.
pub fn note_tool_activity() {
    if let Some(f) = TOOL_ACTIVITY_FN.get() {
        f();
    }
}

/// Whether an action on a process is about to run or has returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionPhase {
    Started,
    Finished,
}

type ActionActivityFn = Box<dyn Fn(i64, ActionPhase) + Send + Sync>;
static ACTION_ACTIVITY_FN: OnceLock<ActionActivityFn> = OnceLock::new();

/// Register the action callback: the registry calls it around every action
/// tool dispatched against a process (`pid`), so a platform can tell a later
/// read of that process to wait for what the action set off (macOS does).
/// Returns false when one is already set; the first registration wins.
pub fn set_action_activity_fn(f: impl Fn(i64, ActionPhase) + Send + Sync + 'static) -> bool {
    ACTION_ACTIVITY_FN.set(Box::new(f)).is_ok()
}

/// Report an action on `pid` starting or ending. Must stay cheap.
pub fn note_action_activity(pid: i64, phase: ActionPhase) {
    if let Some(f) = ACTION_ACTIVITY_FN.get() {
        f(pid, phase);
    }
}

/// One action on a process, from its start until this is dropped. Dropping
/// reports it finished however the action ends: returned, cancelled (its
/// future dropped) or panicked. Otherwise the process's next read would wait
/// for an end that never comes.
#[must_use = "the action is reported finished when this is dropped"]
pub struct ActionActivity {
    pid: i64,
}

impl ActionActivity {
    /// Report an action on `pid` starting.
    pub fn start(pid: i64) -> Self {
        note_action_activity(pid, ActionPhase::Started);
        Self { pid }
    }
}

impl Drop for ActionActivity {
    fn drop(&mut self) {
        note_action_activity(self.pid, ActionPhase::Finished);
    }
}
