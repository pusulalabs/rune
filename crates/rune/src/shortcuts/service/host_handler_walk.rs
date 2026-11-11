//! Handlers against the hosts that took their registrations, in every interleaving of two hosts
//! and one caller, each split where the code takes and lets go of a lock. An attach puts its
//! engine in place and then waits for routing. A close takes routing, then the engine (on Linux
//! under the slot's lock, which it holds until it has the engine's), then cleans up. A
//! registration reserves its id and handler, is routed, then reaches an engine; a caller whose
//! queued call a close let go finishes on its own thread, later. A refused caller takes its
//! handler out, drops it, then returns: on dev under the handler lock, which a close waits for;
//! on the new side outside it, with a close waiting for the drop. A close takes and drops the
//! handlers it ends in one step, since nothing else drops while it waits for a running drop.
//!
//! Three sides play each sequence: the code on dev, that code with the marked departure, and the
//! new code, whose handlers are the production `Handlers` and `HostIds`. All three route through
//! the real machine, which this change leaves alone. The new code matches the marked reference
//! at every event, and the reference matches dev up to the event where the departure first shows.

use super::*;
use std::{collections::{BTreeMap, BTreeSet}, sync::LazyLock, thread::ThreadId};

const A: usize = 0;
const B: usize = 1;
/// The thread of a caller that is neither host.
const OTHER: usize = 2;

static THREADS: LazyLock<[ThreadId; 3]> = LazyLock::new(|| [(); 3].map(|()| std::thread::spawn(|| std::thread::current().id()).join().unwrap()));

const DROPPED_LATER: &str = "a cleanup dropped a handler routed after its close took routing and not released by it";
const KEPT_EARLIER: &str = "a cleanup kept a handler routed before its close took routing";
const KEPT_RELEASED: &str = "a cleanup kept the handler of a registration its close released";
const LOST_LIVE: &str = "the attached host's engine holds a registration with no handler";
const LEAKED: &str = "a handler outlived both hosts";
const CAPTURE_OUTLIVES_CLOSE: &str = "a close returned while a handler it ends was still dropping";
const KEEPS_LATER: &str = "a cleanup kept a later host's handlers";
const REFUSED_CLOSED: &str = "a bind reaching an engine a close took was refused";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side { Dev, Reference, New }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    Unborn,
    /// Its engine is in place; its attach waits for routing.
    Installed,
    Attached,
    /// Its close took routing as the `epoch`th close, when `latest` was the latest host. On
    /// Linux `taken` is the engine it took from the slot, once it has; while it waits for that
    /// engine's lock it holds the slot's.
    Closing { epoch: usize, latest: HostId, taken: Option<Option<usize>> },
    /// Its close let go of the engine too, which held the ids in the `released` bitmask.
    Released { epoch: usize, latest: HostId, released: u64 },
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Reserved,
    /// Holding routing while it waits for the slot's lock.
    SlotWait,
    /// Holding routing while it waits for this engine's lock.
    Parked(usize),
    /// Waiting for an owner thread to drain it.
    Queued,
    /// A close let its queued call go; the caller hasn't seen NotRunning yet.
    LetGo,
    /// Refused, it took its handler out and is dropping it before it returns NotRunning.
    Dropping,
    /// On its way into `engine`, holding its lock when `locked`.
    Routed { engine: usize, locked: bool },
}

#[derive(Clone, Default)]
struct EngineState { held: BTreeSet<u32>, released: bool }

struct World {
    side: Side,
    linux: bool,
    lives: [Life; 2],
    engines: Vec<EngineState>,
    slot: Option<usize>,
    locals: [Option<usize>; 2],
    attachment: Attachment<u32>,
    hosts: HostIds,
    /// The handlers on dev and the reference.
    ids: BTreeSet<u32>,
    /// The handlers on the new side.
    table: Handlers<()>,
    /// How many closes had taken routing when each registration was routed.
    routed_after: BTreeMap<u32, usize>,
    closes: usize,
    call: Phase,
    id: u32,
    result: Option<Result<(), ShortcutError>>,
    /// Another call inside this engine, holding its lock.
    busy: Option<usize>,
    /// A host thread in the middle of the registration's call, which can do nothing else until
    /// the call returns.
    occupied: Option<usize>,
    /// The marked departure this event took, if any.
    departure: Option<&'static str>,
    fired: BTreeSet<u32>,
    broken: Vec<&'static str>,
    /// What the last cleanup kept, and what keeping only the registrations neither routed before
    /// its close took routing nor released by it would have kept.
    kept: Option<(BTreeSet<u32>, BTreeSet<u32>)>,
    /// A cleanup ended a registration routed after its close, because the close released it.
    released_later: bool,
}

impl Clone for World {
    fn clone(&self) -> Self {
        let attachment = match &self.attachment {
            Attachment::Detached => Attachment::Detached,
            Attachment::Attached { owner, wake, jobs } => Attachment::Attached { owner: *owner, wake: wake.clone(), jobs: jobs.clone() },
            Attachment::Closed => Attachment::Closed,
        };
        World {
            side: self.side, linux: self.linux, lives: self.lives, engines: self.engines.clone(), slot: self.slot, locals: self.locals,
            attachment, hosts: self.hosts, ids: self.ids.clone(), table: self.table.clone(), routed_after: self.routed_after.clone(),
            closes: self.closes, call: self.call, id: self.id, result: self.result.clone(), busy: self.busy, occupied: self.occupied,
            departure: self.departure, fired: self.fired.clone(),
            broken: self.broken.clone(), kept: self.kept.clone(), released_later: self.released_later,
        }
    }
}

/// Everything an app could tell apart, with each host reduced to its phase.
type View = (Phase, Option<Result<(), ShortcutError>>, [u8; 2], Option<usize>, Vec<u32>, Vec<(usize, Vec<u32>)>, Option<usize>, Vec<u32>, Vec<(u32, bool)>);

fn mask(ids: impl IntoIterator<Item = u32>) -> u64 { ids.into_iter().fold(0, |mask, id| mask | 1 << id) }

fn in_mask(mask: u64, id: u32) -> bool { mask & 1 << id != 0 }

impl World {
    fn new(side: Side, linux: bool) -> Self {
        let mut world = World {
            side, linux, lives: [Life::Unborn; 2], engines: vec![], slot: None, locals: [None; 2], attachment: Attachment::Detached,
            hosts: HostIds::default(), ids: BTreeSet::new(), table: Handlers::default(), routed_after: BTreeMap::new(), closes: 0,
            call: Phase::Idle, id: 0, result: None, busy: None, occupied: None, departure: None, fired: BTreeSet::new(), broken: vec![], kept: None, released_later: false,
        };
        world.install(A);
        world.attach(A);
        world
    }

    fn handler_ids(&self) -> BTreeSet<u32> {
        if self.side == Side::New { self.table.ids().into_iter().map(|id| id.0).collect() } else { self.ids.clone() }
    }

    fn has_handler(&self, id: u32) -> bool {
        if self.side == Side::New { self.table.contains_key(&ShortcutId(id)) } else { self.ids.contains(&id) }
    }

    fn routing_held(&self) -> bool { matches!(self.call, Phase::Parked(_) | Phase::SlotWait) }

    /// A Linux close holds the slot's lock while it waits for the engine's.
    fn slot_held(&self) -> bool { self.lives.iter().any(|life| matches!(life, Life::Closing { taken: Some(Some(_)), .. })) }

    fn locked(&self, engine: usize) -> bool { self.busy == Some(engine) || self.call == Phase::Routed { engine, locked: true } }

    fn owner(&self) -> Option<usize> {
        let Attachment::Attached { owner, .. } = &self.attachment else { return None };
        THREADS.iter().position(|thread| thread == owner)
    }

    fn jobs(&self) -> Vec<u32> {
        let Attachment::Attached { jobs, .. } = &self.attachment else { return vec![] };
        jobs.clone()
    }

    /// The engine the attached host's calls reach, while it is live.
    fn current_engine(&self) -> Option<usize> {
        let owner = self.owner()?;
        let engine = if self.linux { self.slot } else { self.locals[owner] }?;
        (!self.engines[engine].released).then_some(engine)
    }

    fn install(&mut self, host: usize) {
        self.engines.push(EngineState::default());
        let engine = self.engines.len() - 1;
        if self.linux { self.slot = Some(engine) } else { self.locals[host] = Some(engine) }
        self.lives[host] = Life::Installed;
    }

    fn attach(&mut self, host: usize) {
        self.hosts.attach();
        step_attachment(&mut self.attachment, AttachEvent::Attach { owner: THREADS[host], wake: Arc::new(|| {}) });
        self.lives[host] = Life::Attached;
    }

    fn close_routing(&mut self, host: usize) {
        let jobs = match step_attachment(&mut self.attachment, AttachEvent::Close).into_iter().next() {
            Some(AttachEffect::Release(jobs)) => jobs,
            _ => vec![],
        };
        // The close drops a queued caller's reply sender; the caller wakes to NotRunning later.
        // The canceled call was the closing host's, routed before this close.
        if self.call == Phase::Queued && jobs.contains(&self.id) {
            self.mark_routed();
            self.call = Phase::LetGo;
        }
        self.closes += 1;
        self.lives[host] = Life::Closing { epoch: self.closes, latest: self.hosts.latest(), taken: None };
    }

    /// Lets go of the engine. On Linux the close takes the slot's engine, whichever it is, then
    /// waits for its lock to release it, holding the slot's lock meanwhile.
    fn release_engine(&mut self, host: usize) -> bool {
        let Life::Closing { epoch, latest, taken } = self.lives[host] else { unreachable!() };
        let engine = if !self.linux {
            self.locals[host].take()
        } else {
            if taken.is_none() && self.slot_held() { return false; }
            let engine = match taken { Some(engine) => engine, None => self.slot.take() };
            if let Some(engine) = engine && self.locked(engine) {
                self.lives[host] = Life::Closing { epoch, latest, taken: Some(Some(engine)) };
                return taken.is_none();
            }
            if let Some(engine) = engine { self.engines[engine].released = true; }
            engine
        };
        let released = engine.map_or(0, |engine| mask(self.engines[engine].held.iter().copied()));
        self.lives[host] = Life::Released { epoch, latest, released };
        true
    }

    fn cleanup(&mut self, host: usize) {
        let Life::Released { epoch, latest, released } = self.lives[host] else { unreachable!() };
        let before = self.handler_ids();
        let routed = self.routed_after.clone();
        let routed_before = |id: &u32| routed.get(id).is_some_and(|after| *after < epoch);
        let ends = |id: &u32| routed_before(id) || in_mask(released, *id);
        match self.side {
            Side::Dev => self.ids.clear(),
            // differs from dev: a cleanup keeps what was routed after its close took routing,
            // unless its close released the engine holding it, and what isn't routed yet. Dev
            // dropped every handler, a later host's too.
            Side::Reference => self.ids.retain(|id| !ends(id)),
            Side::New => {
                let released: Vec<ShortcutId> = (0..64).filter(|id| in_mask(released, *id)).map(ShortcutId).collect();
                for (id, _) in self.table.close(latest, &released) { self.table.dropped(id); }
            }
        }
        let after = self.handler_ids();
        if before.difference(&after).any(|id| !ends(id)) { self.broken.push(DROPPED_LATER); }
        if after.iter().any(routed_before) { self.broken.push(KEPT_EARLIER); }
        if after.iter().any(|id| in_mask(released, *id)) { self.broken.push(KEPT_RELEASED); }
        if self.call == Phase::Dropping && ends(&self.id) { self.broken.push(CAPTURE_OUTLIVES_CLOSE); }
        if before.iter().any(|id| in_mask(released, *id) && !routed_before(id)) { self.released_later = true; }
        let expected: BTreeSet<u32> = before.iter().copied().filter(|id| !ends(id)).collect();
        if !expected.is_empty() { self.departure = Some(KEEPS_LATER); }
        self.kept = Some((after, expected));
        self.lives[host] = Life::Closed;
    }

    fn reserve(&mut self) {
        self.id += 1;
        if self.side == Side::New { let _ = self.table.reserve(ShortcutId(self.id), ()); } else { self.ids.insert(self.id); }
        self.call = Phase::Reserved;
        self.result = None;
    }

    /// Refused: the caller takes its handler out, if it is still there, to drop it.
    fn refuse(&mut self) {
        self.occupied = None;
        let taken = if self.side == Side::New { self.table.take(ShortcutId(self.id)).is_some() } else { self.ids.remove(&self.id) };
        if taken { self.call = Phase::Dropping } else { self.finish_refusal() }
    }

    fn finish_refusal(&mut self) {
        if self.side == Side::New { self.table.dropped(ShortcutId(self.id)); }
        self.call = Phase::Idle;
        self.result = Some(Err(ShortcutError::NotRunning));
    }

    /// A close waits for a running drop: on dev for the handler lock, on the new side for the
    /// table to say nothing is taken.
    fn cleanup_waits(&self) -> bool {
        if self.side == Side::New { self.table.busy() } else { self.call == Phase::Dropping }
    }

    /// Routing hands the call to the latest host.
    fn mark_routed(&mut self) {
        self.routed_after.insert(self.id, self.closes);
        if self.side == Side::New { self.table.routed(ShortcutId(self.id), self.hosts.latest()); }
    }

    /// A Linux call holding routing reads the slot and goes for that engine's lock.
    fn take_slot(&mut self) {
        let Some(engine) = self.slot else { return self.refuse() };
        self.mark_routed();
        self.call = if self.locked(engine) { Phase::Parked(engine) } else { Phase::Routed { engine, locked: true } };
    }

    fn route(&mut self, from: usize) {
        if self.linux {
            match step_attachment(&mut self.attachment, AttachEvent::Call { from: THREADS[OTHER], shared: true }).into_iter().next() {
                Some(AttachEffect::Run { .. }) if self.slot_held() => self.call = Phase::SlotWait,
                Some(AttachEffect::Run { .. }) => self.take_slot(),
                _ => self.refuse(),
            }
            return;
        }
        let run = match step_attachment(&mut self.attachment, AttachEvent::Call { from: THREADS[from], shared: false }).into_iter().next() {
            Some(AttachEffect::Run { .. }) => { self.occupied = (from != OTHER).then_some(from); from }
            Some(AttachEffect::Wait) => {
                step_attachment(&mut self.attachment, AttachEvent::Post(self.id));
                self.call = Phase::Queued;
                return;
            }
            _ => return self.refuse(),
        };
        self.mark_routed();
        match self.locals[run] {
            Some(engine) => self.call = Phase::Routed { engine, locked: false },
            None => self.refuse(),
        }
    }

    fn drain(&mut self, host: usize) {
        let jobs = match step_attachment(&mut self.attachment, AttachEvent::Drain).into_iter().next() {
            Some(AttachEffect::Jobs(jobs)) => jobs,
            _ => vec![],
        };
        if self.call != Phase::Queued || !jobs.contains(&self.id) { return; }
        self.occupied = Some(host);
        self.mark_routed();
        match self.locals[host] {
            Some(engine) => self.call = Phase::Routed { engine, locked: false },
            None => self.refuse(),
        }
    }

    fn run(&mut self) {
        let Phase::Routed { engine, .. } = self.call else { unreachable!() };
        // Only a Linux close releases an engine a call can still reach.
        if self.side != Side::Dev && self.engines[engine].released {
            // differs from dev: a bind reaching an engine a close took is refused, where dev bound
            // it there and returned Ok with its handler already gone.
            self.departure = Some(REFUSED_CLOSED);
            return self.refuse();
        }
        self.occupied = None;
        self.engines[engine].held.insert(self.id);
        self.call = Phase::Idle;
        self.result = Some(Ok(()));
    }

    /// An engine nothing holds any more is dropped, which releases it.
    fn settle(&mut self) {
        let mut reachable: Vec<usize> = self.slot.into_iter().chain(self.locals.into_iter().flatten()).chain(self.busy).collect();
        if let Phase::Parked(engine) | Phase::Routed { engine, .. } = self.call { reachable.push(engine); }
        for life in self.lives { if let Life::Closing { taken: Some(Some(engine)), .. } = life { reachable.push(engine); } }
        for (index, engine) in self.engines.iter_mut().enumerate() {
            if !reachable.contains(&index) { engine.released = true; }
        }
    }

    fn observe(&mut self) {
        self.settle();
        for engine in &self.engines { if !engine.released { self.fired.extend(&engine.held); } }
        if let Some(engine) = self.current_engine() && self.engines[engine].held.iter().any(|id| !self.has_handler(*id)) {
            self.broken.push(LOST_LIVE);
        }
        let ended = |life: Life| matches!(life, Life::Closed | Life::Unborn);
        if self.lives[A] == Life::Closed && ended(self.lives[B]) && self.call == Phase::Idle && !self.handler_ids().is_empty() { self.broken.push(LEAKED); }
    }

    /// Letters: 0 B installs its engine, 1 B's attach takes routing; 2, 3, 4 A's close takes
    /// routing, lets go of the engine, cleans up; 5, 6, 7 the same for B; 8 reserve an id and
    /// handler; 9 route it from another thread; 10 route it from the attached host's thread; 11
    /// it reaches its engine, or a let-go caller sees NotRunning; 12, 13 A or B drains; 14, 15
    /// another call enters an engine and leaves; 16 a waiting call takes the slot or the engine;
    /// 17 a refused caller's handler finishes dropping.
    /// Returns whether the letter did anything.
    fn play(&mut self, letter: usize) -> bool {
        self.kept = None;
        self.departure = None;
        let host = if (2..5).contains(&letter) || letter == 12 { A } else { B };
        // A host's own letters wait while its thread is busy in the registration's call.
        if matches!(letter, 0..=7 | 12 | 13) && self.occupied == Some(host) { return false; }
        let moved = match letter {
            // An install waiting for the slot's lock holds nothing, so it simply comes later.
            0 if self.lives[B] == Life::Unborn && !self.slot_held() => { self.install(B); true }
            1 if self.lives[B] == Life::Installed && !self.routing_held() => { self.attach(B); true }
            2 | 5 if self.lives[host] == Life::Attached && !self.routing_held() => { self.close_routing(host); true }
            3 | 6 if matches!(self.lives[host], Life::Closing { .. }) => self.release_engine(host),
            4 | 7 if matches!(self.lives[host], Life::Released { .. }) && !self.cleanup_waits() => { self.cleanup(host); true }
            8 if self.call == Phase::Idle => { self.reserve(); true }
            9 if self.call == Phase::Reserved => { self.route(OTHER); true }
            10 if self.call == Phase::Reserved && !self.linux => { self.route(self.owner().unwrap_or(OTHER)); true }
            11 if matches!(self.call, Phase::Routed { .. }) => { self.run(); true }
            11 if self.call == Phase::LetGo => { self.refuse(); true }
            12 | 13 if !self.linux && self.lives[host] == Life::Attached && !self.jobs().is_empty() => { self.drain(host); true }
            // A call that would wait for a held slot holds routing; the walk leaves those out,
            // as only the registration's calls are being followed.
            14 if self.linux && self.busy.is_none() && !self.routing_held() && !self.slot_held() && self.owner().is_some()
                && self.slot.is_some_and(|engine| !self.locked(engine)) => {
                self.busy = self.slot;
                true
            }
            15 if self.busy.is_some() => { self.busy = None; true }
            17 if self.call == Phase::Dropping => { self.finish_refusal(); true }
            16 => match self.call {
                Phase::SlotWait if !self.slot_held() => { self.take_slot(); true }
                Phase::Parked(engine) if !self.locked(engine) => { self.call = Phase::Routed { engine, locked: true }; true }
                _ => false,
            },
            _ => false,
        };
        if moved { self.observe(); }
        moved
    }

    fn view(&self) -> View {
        let phase = |life: Life| match life {
            Life::Unborn => 0, Life::Installed => 1, Life::Attached => 2,
            Life::Closing { taken: None, .. } => 3, Life::Closing { .. } => 4, Life::Released { .. } => 5, Life::Closed => 6,
        };
        let live = self.engines.iter().enumerate().filter(|(_, engine)| !engine.released).map(|(index, engine)| (index, engine.held.iter().copied().collect())).collect();
        let deliveries = self.fired.iter().map(|id| (*id, self.has_handler(*id))).collect();
        (self.call, self.result.clone(), self.lives.map(phase), self.owner(), self.jobs(), live, self.busy, self.handler_ids().into_iter().collect(), deliveries)
    }
}

/// The view without the handlers, and what delivering each activation would do.
fn without_handlers(view: View) -> View {
    let (call, result, lives, owner, jobs, live, busy, _, _) = view;
    (call, result, lives, owner, jobs, live, busy, vec![], vec![])
}

struct Walk { found: BTreeSet<&'static str>, departures: BTreeSet<&'static str>, released_later: bool, sequences: usize, waited: bool }

fn walk(sides: &[World; 3], departed: bool, sequence: &mut Vec<usize>, depth: usize, out: &mut Walk) {
    for letter in 0..18 {
        let mut next = sides.clone();
        let waits = next[2].lives.iter().any(|life| matches!(life, Life::Released { .. })) && next[2].cleanup_waits();
        let moved = next.each_mut().map(|world| world.play(letter));
        assert!(next[2].broken.is_empty(), "{:?}: linux {}, {sequence:?} + {letter}", next[2].broken, next[2].linux);
        // Past a departure, dev can be where the others aren't; the reference and new code never.
        assert_eq!(moved[1], moved[2], "{sequence:?} + {letter}: the reference and new code disagree on what can happen");
        assert!(departed || moved[0] == moved[1], "{sequence:?} + {letter}: dev and the reference disagree on what can happen");
        if !moved[1] { continue; }
        sequence.push(letter);
        out.sequences += 1;
        out.waited |= waits;
        let [dev, reference, new] = &next;
        let at = || format!("linux {}, {sequence:?}", new.linux);
        assert!(reference.broken.is_empty(), "reference {:?}: {}", reference.broken, at());
        out.found.extend(dev.broken.iter().copied());
        out.released_later |= new.released_later;
        assert_eq!(new.view(), reference.view(), "{}", at());
        let mut now_departed = departed;
        if !departed && dev.view() != reference.view() {
            // differs from dev: only at one of the two marked departures.
            match reference.departure {
                Some(KEEPS_LATER) => {
                    // A cleanup keeps what was neither routed before its close took routing nor
                    // released by it, where dev kept nothing.
                    assert_eq!(without_handlers(dev.view()), without_handlers(reference.view()), "{}", at());
                    let (kept, expected) = reference.kept.clone().unwrap();
                    assert!(dev.kept.as_ref().is_some_and(|(kept, _)| kept.is_empty()), "{}", at());
                    assert!(!kept.is_empty() && kept == expected, "{}: kept {kept:?}, expected {expected:?}", at());
                }
                Some(REFUSED_CLOSED) => {
                    // The bind is refused, and its handler goes, where dev returned Ok.
                    assert_eq!(dev.result, Some(Ok(())), "{}", at());
                    assert!(reference.call == Phase::Dropping || reference.result == Some(Err(ShortcutError::NotRunning)), "{}", at());
                    let (_, _, lives, owner, jobs, live, busy, _, _) = reference.view();
                    let (_, _, dev_lives, dev_owner, dev_jobs, dev_live, dev_busy, _, _) = dev.view();
                    assert_eq!((lives, owner, jobs, live, busy), (dev_lives, dev_owner, dev_jobs, dev_live, dev_busy), "{}", at());
                    assert!(!reference.has_handler(reference.id), "{}", at());
                }
                _ => panic!("dev and the reference differ outside a marked departure: {}", at()),
            }
            now_departed = true;
            out.departures.extend(reference.departure);
        }
        if sequence.len() < depth { walk(&next, now_departed, sequence, depth, out); }
        sequence.pop();
    }
}

#[test]
fn every_interleaving_keeps_handlers_with_the_hosts_that_took_them() {
    for linux in [true, false] {
        let sides = [Side::Dev, Side::Reference, Side::New].map(|side| World::new(side, linux));
        let mut out = Walk { found: BTreeSet::new(), departures: BTreeSet::new(), released_later: false, sequences: 0, waited: false };
        walk(&sides, false, &mut vec![], 8, &mut out);
        for invariant in [DROPPED_LATER, LOST_LIVE] {
            assert!(out.found.contains(invariant), "linux {linux}: the walk never saw dev break this, so it proves nothing: {invariant}");
        }
        assert!(out.departures.contains(KEEPS_LATER), "linux {linux}: the kept-handlers departure never took effect");
        if linux {
            // From A's close taking routing, B attaching and a call entering B's engine, deep
            // enough for a close to take that engine while a registration waits for it.
            let mut sides = sides.clone();
            let prefix = vec![2, 0, 1, 14];
            for letter in &prefix { assert!(sides.each_mut().map(|world| world.play(*letter)).iter().all(|moved| *moved)); }
            walk(&sides, false, &mut prefix.clone(), prefix.len() + 7, &mut out);
            assert!(out.departures.contains(REFUSED_CLOSED), "the refused-bind departure never took effect");
        }
        assert!(out.waited, "linux {linux}: no close ever had to wait for a refused caller's drop");
        eprintln!("linux {linux}: {} sequences", out.sequences);
        if linux { assert!(out.released_later, "the walk never had a close release a later host's registration"); }
    }
}

#[test]
fn an_attach_waits_while_a_parked_call_holds_routing() {
    let mut world = World::new(Side::New, true);
    // Another call is inside A's engine; a registration parks behind it, holding routing.
    for letter in [14, 8, 9] { assert!(world.play(letter)); }
    assert_eq!(world.call, Phase::Parked(0));
    assert!(world.play(0), "B's engine goes in the slot under the slot's own lock");
    assert!(!world.play(1), "B's attach can't take routing from the parked call");
    assert!(!world.play(2), "nor can A's close");
    assert_eq!((world.lives[B], world.owner()), (Life::Installed, Some(A)));
    assert!(world.play(15) && world.play(16), "the call inside leaves and the parked one takes A's engine");
    assert!(world.play(1), "routing is free again");
    assert!(world.play(11));
    assert_eq!(world.table.host(ShortcutId(1)), Some(Some(HostIds::after(0).attach())), "the parked call went to the host routing had when it was routed");
}

#[test]
fn a_close_waiting_for_a_busy_engine_keeps_the_slot_locked() {
    for side in [Side::Dev, Side::New] {
        let mut world = World::new(side, true);
        // A call is inside A's engine; A's close takes the engine from the slot and waits for it.
        for letter in [14, 2, 3] { assert!(world.play(letter)); }
        assert!(world.slot_held());
        assert!(!world.play(0), "{side:?}: B can't install while the close holds the slot");

        let mut world = World::new(side, true);
        // A's close takes routing; B attaches; a call is inside B's engine when A's close takes it.
        for letter in [2, 0, 1, 14, 3] { assert!(world.play(letter)); }
        assert!(world.slot_held());
        assert!(world.play(8) && world.play(9), "{side:?}");
        assert_eq!(world.call, Phase::SlotWait, "{side:?}: a registration holds routing, waiting for the slot");
        assert!(!world.play(5) && !world.play(16), "{side:?}: so B can't close, and the registration can't go on");
        assert!(world.play(15) && world.play(3), "{side:?}: the call inside leaves and the close releases B's engine");
        assert!(world.play(16) && world.play(17), "{side:?}");
        assert_eq!(world.result, Some(Err(ShortcutError::NotRunning)), "{side:?}: the slot is empty by then");
    }
}

#[test]
fn a_caller_let_go_by_a_close_finishes_after_it() {
    for side in [Side::Dev, Side::New] {
        let mut world = World::new(side, false);
        // A registration from another thread queues for A, then A's close lets it go.
        for letter in [8, 9, 2] { assert!(world.play(letter)); }
        assert_eq!((world.call, world.result.clone()), (Phase::LetGo, None), "{side:?}: the caller hasn't run yet");
        assert!(world.play(11) && world.play(17));
        assert_eq!((world.call, world.result.clone()), (Phase::Idle, Some(Err(ShortcutError::NotRunning))), "{side:?}");
        assert!(!world.has_handler(1), "{side:?}");
    }
}

#[test]
fn a_host_busy_in_its_own_call_cannot_close_or_drain_until_it_returns() {
    for side in [Side::Dev, Side::New] {
        let mut world = World::new(side, false);
        // A registers on its own thread, so the call runs on A's stack.
        assert!(world.play(8) && world.play(10));
        assert_eq!(world.occupied, Some(A));
        assert!(!world.play(2) && !world.play(12), "{side:?}: A can't close or drain mid-call");
        assert!(world.play(0) && world.play(1), "{side:?}: B can still install and attach");
        assert!(world.play(11) && world.play(2), "{side:?}: once the call returns, A can close");
    }
}

#[test]
fn a_bind_reaching_an_engine_a_close_took_is_refused() {
    let mut world = World::new(Side::New, true);
    // A's close takes routing; B attaches; a call is inside B's engine and a registration parks
    // behind it; A's close takes B's engine and releases it first.
    for letter in [2, 0, 1, 14, 8, 9] { assert!(world.play(letter)); }
    assert_eq!(world.call, Phase::Parked(1));
    for letter in [3, 15, 3, 16, 11, 17] { assert!(world.play(letter), "{letter}"); }
    assert_eq!(world.result, Some(Err(ShortcutError::NotRunning)));
    assert!(!world.has_handler(1), "its handler went with it");
}

#[test]
fn a_close_waits_for_a_refused_callers_drop() {
    for side in [Side::Dev, Side::New] {
        let mut world = World::new(side, false);
        // A registration queues for A; A's close cancels it and lets go of its engine.
        for letter in [8, 9, 2, 3] { assert!(world.play(letter)); }
        // The caller takes its handler out and starts dropping it.
        assert!(world.play(11));
        assert_eq!(world.call, Phase::Dropping);
        assert!(!world.play(4), "{side:?}: the close can't finish while the handler drops");
        assert!(world.play(17) && world.play(4), "{side:?}");
    }
}
