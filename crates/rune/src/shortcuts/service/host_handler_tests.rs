//! A close removes the handlers of the hosts it ends and leaves a later host's alone, through
//! the real `Guard` and real threads.

use super::*;
use crate::{App, AppHandle, AppSpec, el};

fn app(title: &'static str) -> App<u32> { App::new(AppSpec::new(title, (10.0, 10.0), || 0, |_: &u32, _| el())) }

fn chord(text: &str) -> Shortcut { text.parse().unwrap() }

/// Runs every queued update, and returns how far the handlers moved the count.
fn deliver(app: &App<u32>) -> u32 {
    let mut state = 0;
    for f in app.shared.take().updates { f(&mut state, &mut Tx::default()); }
    state
}

/// A host attached on its own thread.
struct Host { close: mpsc::Sender<()>, entered: mpsc::Receiver<()>, resume: mpsc::Sender<()>, thread: std::thread::JoinHandle<()>, closing: bool }

impl Host {
    /// Attaches on a new thread and runs `then` there. Its close, once asked for, stops before
    /// its cleanup until `resume`.
    fn spawn(app: &App<u32>, then: impl FnOnce(&Arc<Shared<u32>>, &AppHandle<u32>) + Send + 'static) -> Self {
        let (shared, handle) = (app.shared.clone(), app.handle());
        let (attached, is_attached) = mpsc::channel();
        let (close, closes) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel();
        let (resume, resumed) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut guard = Guard::attach(&shared, Arc::new(|| {}), true, false);
            then(&shared, &handle);
            attached.send(()).unwrap();
            closes.recv().unwrap();
            let cleanup = guard.cleanup.take().unwrap();
            guard.cleanup = Some(Box::new(move |latest, released| {
                entered_tx.send(()).unwrap();
                resumed.recv().unwrap();
                cleanup(latest, released);
            }));
            drop(guard);
        });
        is_attached.recv().unwrap();
        Host { close, entered, resume, thread, closing: false }
    }

    /// Closes up to its cleanup, which waits for `finish`.
    fn close_until_cleanup(&mut self) {
        self.close.send(()).unwrap();
        self.entered.recv().unwrap();
        self.closing = true;
    }

    /// Closes, or finishes closing, all the way.
    fn finish(mut self) {
        if !self.closing { self.close_until_cleanup(); }
        self.resume.send(()).unwrap();
        self.thread.join().unwrap();
    }
}

#[test]
fn a_closing_host_leaves_a_later_hosts_handlers_alone() {
    let app = app("Successor");
    let handle = app.handle();
    let mut first = Host::spawn(&app, |_, handle| { handle.register_shortcut(chord("ctrl+alt+g"), |state, _| *state += 100).unwrap(); });
    first.close_until_cleanup();

    let guard = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    let id = handle.register_shortcut(chord("ctrl+alt+h"), |state, _| *state += 1).unwrap();
    first.finish();

    assert_eq!(app.shared.shortcuts.handlers.lock().unwrap().ids(), [id], "only the closed host's handler went");
    app.shared.shortcuts.fire(&chord("ctrl+alt+h")).unwrap();
    assert_eq!(deliver(&app), 1, "the later host's shortcut still reaches its handler");
    drop(guard);
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty());
}

#[test]
fn a_registration_routed_after_a_close_keeps_its_handler() {
    let app = app("Late");
    let mut first = Host::spawn(&app, |_, _| {});
    // The id and handler exist before the close; the registration reaches a host only after it.
    let id = app.shared.shortcuts.reserve(Arc::new(|state: &mut u32, _: &mut Tx| *state += 1)).unwrap();
    first.close_until_cleanup();

    let guard = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    assert_eq!(app.shared.shortcuts.bind(id, chord("ctrl+alt+l")), Ok(id));
    first.finish();

    app.shared.shortcuts.fire(&chord("ctrl+alt+l")).unwrap();
    assert_eq!(deliver(&app), 1, "the host that took the registration keeps its handler");
    drop(guard);
}

#[test]
fn a_registration_routed_before_a_close_loses_its_handler() {
    let app = app("Early");
    let first = Host::spawn(&app, |shared, handle| {
        handle.register_shortcut(chord("ctrl+alt+e"), |state, _| *state += 1).unwrap();
        shared.shortcuts.fire(&chord("ctrl+alt+e")).unwrap();
    });
    first.finish();
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty());
    assert_eq!(deliver(&app), 0, "an activation queued before the close runs nothing after it");
}

#[test]
fn closing_a_later_host_also_ends_the_handlers_of_the_host_it_replaced() {
    let app = app("Replaced");
    let first = Host::spawn(&app, |shared, handle| {
        handle.register_shortcut(chord("ctrl+alt+r"), |state, _| *state += 1).unwrap();
        shared.shortcuts.fire(&chord("ctrl+alt+r")).unwrap();
    });
    drop(Guard::attach(&app.shared, Arc::new(|| {}), true, false));
    assert_eq!(deliver(&app), 0, "the service is closed, so the replaced host's activation runs nothing");
    first.finish();
}

#[test]
fn an_unrouted_registration_outlives_a_close_and_goes_when_refused() {
    let app = app("Unrouted");
    let first = Host::spawn(&app, |_, _| {});
    let id = app.shared.shortcuts.reserve(Arc::new(|state: &mut u32, _: &mut Tx| *state += 1)).unwrap();
    first.finish();
    assert!(app.shared.shortcuts.handlers.lock().unwrap().contains_key(&id), "no host had it, so no close takes it");
    assert_eq!(app.shared.shortcuts.bind(id, chord("ctrl+alt+u")), Err(ShortcutError::NotRunning));
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty(), "refused, it goes with its registration");
}

#[test]
fn a_close_that_releases_a_later_hosts_engine_runs_none_of_its_callbacks() {
    let app = app("Released");
    let handle = app.handle();
    // A's close takes routing; before it lets go of its engine, B attaches, registers and fires.
    let mut first = std::mem::ManuallyDrop::new(Guard::attach(&app.shared, Arc::new(|| {}), true, false));
    let latest = first.close_routing();
    let second = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    handle.register_shortcut(chord("ctrl+alt+x"), |state, _| *state += 1).unwrap();
    app.shared.shortcuts.fire(&chord("ctrl+alt+x")).unwrap();
    // The engine A lets go of is B's (in the slot, or this thread's), and with it B's registration.
    first.finish_close(latest);
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty(), "the released registration's handler went");
    assert_eq!(deliver(&app), 0, "its queued activation runs nothing");
    drop(second);
}

#[test]
fn host_order_holds_past_u32_max() {
    let app = app("Rollover");
    let handle = app.handle();
    app.shared.shortcuts.control.attachment.lock().unwrap().hosts = HostIds::after(u64::from(u32::MAX) - 1);
    let first = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    handle.register_shortcut(chord("ctrl+alt+m"), |state, _| *state += 1).unwrap();
    app.shared.shortcuts.fire(&chord("ctrl+alt+m")).unwrap();
    // B is the host after u32::MAX; closing it ends A, which attached before it.
    drop(Guard::attach(&app.shared, Arc::new(|| {}), true, false));
    assert_eq!(deliver(&app), 0, "A's activation runs nothing once the service is closed");
    drop(first);
}

/// A handler capture whose drop asks who holds a chord, which takes routing.
struct AsksOnDrop { handle: AppHandle<u32>, dropping: mpsc::Sender<()> }

impl Drop for AsksOnDrop {
    fn drop(&mut self) {
        let _ = self.dropping.send(());
        // Long enough for the other registration to take routing and reach the handlers.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = self.handle.shortcut_owner(&chord("ctrl+alt+q"));
    }
}

// Elsewhere a call from another thread waits for the owner rather than taking the handlers.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_handler_drops_after_the_handler_lock_so_routing_can_proceed() {
    let app = app("Drops");
    let handle = app.handle();
    let guard = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    handle.register_shortcut(chord("ctrl+alt+d"), |_, _| {}).unwrap();
    // Reserved now, routed while the refused handler drops.
    let waiting = app.shared.shortcuts.reserve(Arc::new(|_: &mut u32, _: &mut Tx| {})).unwrap();

    let (done, finished) = mpsc::channel();
    let (dropping, is_dropping) = mpsc::channel();
    let refused = {
        let (handle, done) = (handle.clone(), done.clone());
        std::thread::spawn(move || {
            let probe = AsksOnDrop { handle: handle.clone(), dropping };
            let result = handle.register_shortcut(chord("ctrl+alt+d"), move |_, _| { let _ = &probe; });
            done.send(("refused", result == Err(ShortcutError::Conflict))).unwrap();
        })
    };
    is_dropping.recv().unwrap();
    let routed = {
        let shared = app.shared.clone();
        std::thread::spawn(move || done.send(("routed", shared.shortcuts.bind(waiting, chord("ctrl+alt+w")).is_ok())).unwrap())
    };
    let results: Vec<_> = (0..2).map(|_| finished.recv_timeout(std::time::Duration::from_secs(30))).collect();
    if !results.iter().all(|result| matches!(result, Ok((_, true)))) {
        // Closing would wait on the deadlocked routing too, so the test leaves them be to fail.
        std::mem::forget((guard, app));
        panic!("{results:?}: the two deadlocked");
    }
    refused.join().unwrap();
    routed.join().unwrap();
    drop(guard);
}

#[cfg(target_os = "linux")]
#[test]
fn a_bind_holding_an_engine_a_close_took_is_refused() {
    let app = app("Taken");
    // A's close takes routing; B attaches; a call already holds B's engine when A's close takes it.
    let mut first = std::mem::ManuallyDrop::new(Guard::attach(&app.shared, Arc::new(|| {}), true, false));
    let latest = first.close_routing();
    let second = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    let held = app.shared.shortcuts.control.engine.lock().unwrap().clone().unwrap();
    first.finish_close(latest);
    assert_eq!(held.lock().unwrap().register(ShortcutId(u32::MAX - 3), chord("ctrl+alt+t")), Err(ShortcutError::NotRunning));
    assert!(held.lock().unwrap().bindings.ids().is_empty(), "nothing bound on the taken engine");
    drop(second);
}

/// Blocks a caller in its wake until `resume`, after saying so on `entered`.
#[cfg(not(target_os = "linux"))]
fn pausing_wake() -> (Waker, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered) = mpsc::channel();
    let (resume, resumed) = mpsc::channel::<()>();
    let (entered_tx, resumed) = (Mutex::new(entered_tx), Mutex::new(resumed));
    let wake: Waker = Arc::new(move || {
        entered_tx.lock().unwrap().send(()).unwrap();
        let _ = resumed.lock().unwrap().recv();
    });
    (wake, entered, resume)
}

// On Linux nothing queues for the owner.
#[cfg(not(target_os = "linux"))]
#[test]
fn a_close_drops_the_handler_of_a_call_it_canceled() {
    let app = app("Canceled");
    let (wake, entered, resume) = pausing_wake();
    let guard = Guard::attach(&app.shared, wake, true, false);
    let probe = Arc::new(());
    let weak = Arc::downgrade(&probe);
    let caller = {
        let handle = app.handle();
        std::thread::spawn(move || handle.register_shortcut(chord("ctrl+alt+c"), move |_, _| { let _ = &probe; }))
    };
    // The call is queued for the owner, and its caller is stopped in the wake, before its reply.
    entered.recv().unwrap();
    drop(guard);
    let alive = weak.upgrade().is_some();
    resume.send(()).unwrap();
    assert_eq!(caller.join().unwrap(), Err(ShortcutError::NotRunning));
    assert!(!alive, "the canceled call's handler went by the time the close returned");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn an_unregister_from_another_thread_drops_the_handler_on_the_owner() {
    struct RecordsThread(Arc<Mutex<Option<std::thread::ThreadId>>>);
    impl Drop for RecordsThread {
        fn drop(&mut self) { *self.0.lock().unwrap() = Some(std::thread::current().id()); }
    }
    let app = app("Owner");
    let (woke_tx, woke) = mpsc::channel();
    let woke_tx = Mutex::new(woke_tx);
    let guard = Guard::attach(&app.shared, Arc::new(move || { let _ = woke_tx.lock().unwrap().send(()); }), true, false);
    let dropped_on = Arc::new(Mutex::new(None));
    let probe = RecordsThread(dropped_on.clone());
    let id = app.handle().register_shortcut(chord("ctrl+alt+o"), move |_, _| { let _ = &probe; }).unwrap();
    while woke.try_recv().is_ok() {}
    let caller = { let handle = app.handle(); std::thread::spawn(move || handle.unregister_shortcut(id)) };
    woke.recv().unwrap();
    guard.drain();
    assert_eq!(caller.join().unwrap(), Ok(()));
    assert_eq!(*dropped_on.lock().unwrap(), Some(std::thread::current().id()), "the capture dropped on the owner, which ran the call");
    drop(guard);
}

/// A capture that, when dropped, says so and waits to be let go.
struct PausesOnDrop { dropping: mpsc::Sender<()>, resume: Mutex<mpsc::Receiver<()>> }

impl Drop for PausesOnDrop {
    fn drop(&mut self) {
        let _ = self.dropping.send(());
        let _ = self.resume.lock().unwrap().recv();
    }
}

#[test]
fn a_close_returns_only_once_a_refused_callers_handler_has_dropped() {
    let app = app("Dropping");
    let chord = chord("ctrl+alt+k");
    // The host runs on its own thread, closing in two halves when told.
    let (ready, is_ready) = mpsc::channel();
    let (step, steps) = mpsc::channel::<()>();
    let (closed, is_closed) = mpsc::channel();
    let host = {
        let (shared, handle, chord) = (app.shared.clone(), app.handle(), chord.clone());
        std::thread::spawn(move || {
            let woke = Mutex::new(ready.clone());
            let mut guard = std::mem::ManuallyDrop::new(Guard::attach(&shared, Arc::new(move || { let _ = woke.lock().unwrap().send(()); }), true, false));
            // On Linux the caller's duplicate is refused outright.
            if cfg!(target_os = "linux") { handle.register_shortcut(chord, |_, _| {}).unwrap(); }
            ready.send(()).unwrap();
            steps.recv().unwrap();
            let latest = guard.close_routing();
            steps.recv().unwrap();
            guard.finish_close(latest);
            closed.send(()).unwrap();
        })
    };
    is_ready.recv().unwrap();
    while is_ready.try_recv().is_ok() {}

    let (dropping, is_dropping) = mpsc::channel();
    let (resume, resumed) = mpsc::channel::<()>();
    let probe = PausesOnDrop { dropping, resume: Mutex::new(resumed) };
    let caller = {
        let handle = app.handle();
        std::thread::spawn(move || handle.register_shortcut(chord, move |_, _| { let _ = &probe; }))
    };
    if cfg!(not(target_os = "linux")) {
        // Elsewhere the call queues for the host, and the close cancels it.
        is_ready.recv().unwrap();
        step.send(()).unwrap();
    }
    // The caller has taken its handler out and is dropping it.
    is_dropping.recv().unwrap();
    if cfg!(target_os = "linux") { step.send(()).unwrap(); }
    step.send(()).unwrap();
    let early = is_closed.recv_timeout(std::time::Duration::from_millis(300));
    resume.send(()).unwrap();
    let returned = is_closed.recv_timeout(std::time::Duration::from_secs(30));
    let answer = caller.join().unwrap();
    host.join().unwrap();
    assert!(early.is_err(), "the close returned while the handler it cleared was still dropping");
    assert!(returned.is_ok(), "the close returns once it has dropped");
    assert!(answer.is_err(), "{answer:?}");
}
