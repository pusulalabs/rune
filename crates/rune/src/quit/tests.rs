use super::*;

/// A quit the system asks for waits for the windows' answer only in an app with an
/// `on_quit_request` window, and goes ahead when no answer can come.
#[test]
fn a_quit_request_waits_for_the_windows_answer() {
    let asks = || ASKS.load(std::sync::atomic::Ordering::Relaxed);
    assert!(asks() || allowed(|| unreachable!("no window asks, so nothing waits")));
    asks_windows();
    assert!(allowed(|| unreachable!("the loop is gone")));
    ASK.with(|ask| *ask.borrow_mut() = Some(Box::new(|| true)));
    for answered in [true, false] {
        assert_eq!(allowed(|| { assert!(answering()); answer(answered); assert!(!answering()); }), answered);
    }
    assert!(allowed(|| {}), "an answer that never comes lets the quit go ahead");
    answer(false);
    assert!(allowed(|| answer(true)), "the late answer was no answer to the next request");
    let _handling = handling();
    assert!(!allowed(|| unreachable!("a handler runs, so the loop can't ask")), "and the request is refused");
}
