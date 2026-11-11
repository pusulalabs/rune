//! The app quitting, as one signal whatever starts it: `Tx::quit` or `AppHandle::quit` from any
//! window, the last window closing, macOS Quit (?Q, the Dock, logout and shutdown, which AppKit
//! asks the app delegate about with `applicationShouldTerminate:`), and a Windows logoff or
//! shutdown (`WM_QUERYENDSESSION`, then `WM_ENDSESSION`). Every open window's `App::on_quit`
//! runs once when it starts, before any window closes for it, and [`quitting`] holds from then
//! on, so a window's `on_close` can tell the app ending from the user closing that one window.
//!
//! One machine ([`State`], [`step`]) holds where the quit is: running, quitting (the windows'
//! hooks owed, then run, and whether the app starts again after, `Tx::relaunch`), done. The hooks run in the event loop (`Windows`, which carries out
//! the effects). Windows sends its session messages to a window and waits for the answer, and
//! may end the process once every `WM_ENDSESSION` has returned, so the session window (`win32`)
//! wakes the loop and runs it until the hooks have run before it answers; when it comes while
//! a loop handler runs (a modal loop inside one), it can only end that loop and let the hooks
//! run once the handler returns.
//!
//! Before a quit the system asks for starts (macOS `applicationShouldTerminate:`, Windows
//! `WM_QUERYENDSESSION`), a window may refuse it (`App::on_quit_request`). The system waits for
//! that answer too, so the request wakes the loop and runs it where it stands until every
//! window answered ([`allowed`], [`answer`]); a refusal starts no quit. Only an app with such a
//! window asks, so any other answers at once, as before.

use std::sync::Mutex;

#[cfg(target_os = "macos")]
mod mac;
pub(crate) mod relaunch;
#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests;
#[cfg(target_os = "windows")]
mod win32;

/// Where the app's quit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The app runs.
    Running,
    /// A quit started; the windows' hooks are owed or ran, and what follows the exit.
    Quitting(Hooks, After),
    /// The session ends now (a logoff): the windows closed as their close buttons would, and
    /// the loop is ending.
    Closed,
    /// The loop ended.
    Done,
}

/// The windows' `App::on_quit` hooks, for a quit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hooks {
    /// Not run yet.
    Owed,
    /// Every open window ran its hook.
    Ran,
}

/// What follows a quit's exit: nothing, or the app starting again (`Tx::relaunch`). A relaunch
/// asked during a quit outranks the plain exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum After {
    Exit,
    Relaunch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Something asks to quit: `Tx::quit` or `Tx::relaunch`, the last window closing, macOS
    /// terminating, a Windows session query (`ending: false`) or its end (`ending: true`).
    Start { ending: bool, after: After },
    /// The loop ran every window's hook.
    HooksRan,
    /// The loop closed the windows for the session's end (`Effect::CloseAndExit`).
    WindowsClosed,
    /// The system took the quit back (another app refused a logoff): the app runs on.
    Cancel,
    /// The event loop ended.
    Exited,
}

/// What the loop does for a step, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Every open window runs `App::on_quit`, then the loop sends `Event::HooksRan`.
    RunHooks,
    /// Every open window closes as its close button would, the loop sends
    /// `Event::WindowsClosed` and ends.
    CloseAndExit,
    /// The loop ended for a relaunch: start the app again (`crate::relaunch`).
    Relaunch,
}

pub(crate) fn step(state: State, event: Event) -> (State, Vec<Effect>) {
    use {Effect::*, Event::*, State::*};
    let close = |ending: bool| if ending { vec![CloseAndExit] } else { vec![] };
    match (state, event) {
        (Done, _) => (Done, vec![]),
        (Quitting(_, After::Relaunch), Exited) => (Done, vec![Relaunch]),
        (_, Exited) => (Done, vec![]),
        // A session ending closed the windows: the system ends the app, and nothing restarts it.
        (Closed, _) => (Closed, vec![]),
        (Running, Start { ending, after }) => (Quitting(Hooks::Owed, after), [vec![RunHooks], close(ending)].concat()),
        (Quitting(Hooks::Owed, was), Start { ending, after }) => (Quitting(Hooks::Owed, was.max(after)), [vec![RunHooks], close(ending)].concat()),
        (Quitting(Hooks::Ran, was), Start { ending, after }) => (Quitting(Hooks::Ran, was.max(after)), close(ending)),
        (Quitting(_, after), HooksRan) => (Quitting(Hooks::Ran, after), vec![]),
        (Quitting(..), WindowsClosed) => (Closed, vec![]),
        (Running | Quitting(..), Cancel) => (Running, vec![]),
        (Running, HooksRan | WindowsClosed) => (Running, vec![]),
    }
}

static STATE: Mutex<State> = Mutex::new(State::Running);
static FAST_EXIT: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Ends the process without tearing anything down once the app has quit: every window hides as
/// the quit starts, every window's `App::on_quit` runs, the clipboard hands over what it holds,
/// tray icons leave the notification area, a relaunch starts, then `last` runs (the app's final
/// durable write, a trace) and the process ends at once. Windows, GPU devices, threads and memory
/// aren't dropped: the OS takes them back, which on Windows skips hundreds of milliseconds of
/// destructors and graphics-driver `DllMain` detach. Nothing after the event loop returns runs,
/// so whatever the app does there belongs in `last` or its quit hooks. Set it once, before the
/// app runs; a later call does nothing. On the web the page's own lifetime applies instead.
pub fn fast_exit(last: fn()) {
    let _ = FAST_EXIT.set(last);
}

/// The app's last step before a fast exit, when it asked for one (`fast_exit`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fast_exit_hook() -> Option<fn()> {
    FAST_EXIT.get().copied()
}

/// A fast exit, once the windows' hooks ran and the loop is ending: what must leave the system
/// tidy goes in order (the clipboard's hand-over, tray icons, a relaunch), then the app's
/// `last`, then the process ends without dropping a window, a device or an allocation.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn end_fast(last: fn()) -> ! {
    crate::clipboard::shutdown();
    crate::tray::remove_all();
    if send(Event::Exited).contains(&Effect::Relaunch) {
        relaunch::spawn();
    }
    crate::marks::mark("quit.last");
    last();
    end_process()
}

/// Ends this process now with exit code 0: no destructors, no `atexit`, no DLL detach.
#[cfg(not(target_arch = "wasm32"))]
fn end_process() -> ! {
    crate::marks::mark("quit.end");
    #[cfg(target_os = "windows")]
    // SAFETY: the pseudo-handle names this process; TerminateProcess on it doesn't return.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        TerminateProcess(GetCurrentProcess(), 0);
    }
    #[cfg(unix)]
    // SAFETY: `_exit` ends the process without running anything of this process's.
    unsafe { libc::_exit(0) }
    #[allow(unreachable_code)]
    std::process::exit(0)
}

/// Steps the app's quit, returning what the loop must do.
pub(crate) fn send(event: Event) -> Vec<Effect> {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let (next, effects) = step(*state, event);
    if *state == State::Running && matches!(next, State::Quitting(..)) { crate::marks::mark("quit.start"); }
    *state = next;
    effects
}

pub(crate) fn state() -> State { *STATE.lock().unwrap_or_else(|e| e.into_inner()) }

/// Whether the app is quitting: a quit started and the system didn't cancel it. A window's
/// `on_close` that runs while this holds closes because the app ends, so it keeps what it would
/// forget when the user closes it alone (an app reopening its windows next launch keeps that
/// window's place in the list). False while the app runs, including when its last window
/// closes, whose `on_close` runs before the quit starts.
pub fn quitting() -> bool {
    !matches!(state(), State::Running)
}

/// Hooks the system's quit requests into the loop: `send` delivers a quit (`true` when the
/// session ends now and the windows must close) to `Windows`, and `ask` has it ask its windows
/// whether the app may quit ([`allowed`]); each is false once the loop is gone. Once per
/// process, on the UI thread.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn install(send: impl Fn(bool) -> bool + 'static, ask: impl Fn() -> bool + 'static) {
    ASK.with(|a| if let Ok(mut a) = a.try_borrow_mut() { *a = Some(Box::new(ask)); });
    #[cfg(target_os = "windows")]
    win32::install(send);
    #[cfg(target_os = "macos")]
    { let _ = send; mac::install(); }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = send;
}

/// Whether some window has `App::on_quit_request`: only then does a quit request ask.
static ASKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A window with `App::on_quit_request` started: the system's quit requests ask the windows
/// from now on.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn asks_windows() {
    ASKS.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A quit request the system waits on, on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    /// None waits.
    Idle,
    /// The loop is asking its windows.
    Asked,
    /// They answered whether the app may quit.
    Answered(bool),
}

thread_local! {
    /// Has the loop ask its windows whether the app may quit; false once the loop is gone.
    static ASK: std::cell::RefCell<Option<Box<dyn Fn() -> bool>>> = const { std::cell::RefCell::new(None) };
    static REQUEST: std::cell::Cell<Request> = const { std::cell::Cell::new(Request::Idle) };
}

/// How long a quit request runs the loop for the windows' answer before it goes ahead anyway:
/// Windows shows that the app is preventing shutdown after five seconds.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
const WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether the quit the system asks for may go ahead: no window has `App::on_quit_request`, or
/// every window's agrees. `run` runs the loop until [`answering`] is over, as the platform's
/// modal loops do. A request that comes while a loop handler runs (a modal loop inside one)
/// can't ask the windows, so it is refused; one whose answer never comes (the loop is gone, or
/// it took longer than `run` waits) goes ahead, as it would without the hook.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn allowed(run: impl FnOnce()) -> bool {
    if !ASKS.load(std::sync::atomic::Ordering::Relaxed) { return true; }
    if loop_state() == Loop::Handling { return false; }
    let sent = ASK.with(|ask| ask.try_borrow().ok().and_then(|ask| ask.as_ref().map(|ask| ask()))).unwrap_or(false);
    if !sent { return true; }
    REQUEST.set(Request::Asked);
    run();
    match REQUEST.replace(Request::Idle) {
        Request::Answered(allowed) => allowed,
        Request::Idle | Request::Asked => true,
    }
}

/// Whether a quit request still waits for the windows' answer.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn answering() -> bool {
    REQUEST.get() == Request::Asked
}

/// The windows' answer to the quit request that waits ([`allowed`]); an answer nothing waits
/// for any more changes nothing.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn answer(allowed: bool) {
    if REQUEST.get() != Request::Asked { return; }
    REQUEST.set(Request::Answered(allowed));
    #[cfg(target_os = "macos")]
    mac::stop_loop();
}

/// Whether the event loop is inside one of its handlers, on the UI thread: a session message
/// that arrives then (from a modal loop the handler runs) can't run the windows' hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loop {
    Idle,
    Handling,
}

thread_local! {
    static LOOP: std::cell::Cell<Loop> = const { std::cell::Cell::new(Loop::Idle) };
}

/// Marks the loop handling an event until the guard drops.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn handling() -> Handling { Handling(LOOP.replace(Loop::Handling)) }

#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(crate) fn loop_state() -> Loop { LOOP.get() }

pub(crate) struct Handling(Loop);

impl Drop for Handling {
    fn drop(&mut self) { LOOP.set(self.0); }
}
