//! Which host took each shortcut registration, so a close removes only the handlers it ends.

use super::super::ShortcutId;
use std::{collections::HashMap, sync::{Condvar, LockResult, Mutex, MutexGuard}};

/// A host, in the order hosts attached. Never reused, so a later host always compares greater.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct HostId(u64);

/// Issues each attach its `HostId`.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HostIds(u64);

impl HostIds {
    pub(super) fn attach(&mut self) -> HostId {
        self.0 = self.0.checked_add(1).expect("an App attaches fewer than 2^64 hosts");
        HostId(self.0)
    }

    pub(super) fn latest(self) -> HostId { HostId(self.0) }

    /// Counting on from `last`, for tests near a limit.
    #[cfg(test)]
    pub(super) fn after(last: u64) -> Self { HostIds(last) }
}

/// Who owns a registration's handler.
#[derive(Clone)]
enum Slot<H> {
    /// The table, with the host that took the registration: None until it is routed to one.
    Held { host: Option<HostId>, handler: H },
    /// A call or a close took it out to drop it after letting go of the lock around this, since
    /// a handler's captures can run anything when they drop. It stays here until `dropped`, and
    /// a close waits for that, so no handler outlives the close that cleared it.
    Taken,
}

/// Each registration's handler, and who owns it.
#[derive(Clone)]
pub(super) struct Handlers<H> {
    entries: HashMap<ShortcutId, Slot<H>>,
}

impl<H> Default for Handlers<H> {
    fn default() -> Self { Self { entries: HashMap::new() } }
}

impl<H> Handlers<H> {
    /// A registration's handler, before any host has it. Returns what it replaced.
    #[must_use]
    pub(super) fn reserve(&mut self, id: ShortcutId, handler: H) -> Option<H> {
        match self.entries.insert(id, Slot::Held { host: None, handler }) {
            Some(Slot::Held { handler, .. }) => Some(handler),
            _ => None,
        }
    }

    /// Routing handed the registration to `host`.
    pub(super) fn routed(&mut self, id: ShortcutId, host: HostId) {
        if let Some(Slot::Held { host: held, .. }) = self.entries.get_mut(&id) { *held = Some(host); }
    }

    /// Takes the handler out to drop, if the table holds it. Say `dropped` once it is.
    #[must_use]
    pub(super) fn take(&mut self, id: ShortcutId) -> Option<H> {
        let Some(Slot::Held { .. }) = self.entries.get(&id) else { return None };
        let Some(Slot::Held { handler, .. }) = self.entries.insert(id, Slot::Taken) else { unreachable!() };
        Some(handler)
    }

    pub(super) fn dropped(&mut self, id: ShortcutId) {
        if let Some(Slot::Taken) = self.entries.get(&id) { self.entries.remove(&id); }
    }

    /// Whether a handler taken out hasn't been dropped yet.
    pub(super) fn busy(&self) -> bool { self.entries.values().any(|slot| matches!(slot, Slot::Taken)) }

    /// A close that took routing when `latest` was the latest host, and let go of an engine
    /// holding `released`. It ends every host up to `latest`, and the registrations it released,
    /// which can be a later host's when that host's engine was the one it let go of. A later
    /// host keeps the rest, as does a registration no host has taken yet.
    /// Takes them out to drop, as `take` does.
    #[must_use]
    pub(super) fn close(&mut self, latest: HostId, released: &[ShortcutId]) -> Vec<(ShortcutId, H)> {
        let ended: Vec<ShortcutId> = self.entries.iter()
            .filter(|(id, slot)| matches!(slot, Slot::Held { host: Some(host), .. } if *host <= latest) || released.contains(id))
            .map(|(id, _)| *id).collect();
        ended.into_iter().filter_map(|id| Some((id, self.take(id)?))).collect()
    }

    fn held(&self) -> impl Iterator<Item = (&ShortcutId, Option<HostId>, &H)> {
        self.entries.iter().filter_map(|(id, slot)| match slot { Slot::Held { host, handler } => Some((id, *host, handler)), Slot::Taken => None })
    }

    pub(super) fn contains_key(&self, id: &ShortcutId) -> bool { self.held().any(|(held, ..)| held == id) }

    /// Nothing held and nothing being dropped.
    pub(super) fn is_empty(&self) -> bool { self.entries.is_empty() }

    #[cfg(test)]
    pub(super) fn ids(&self) -> Vec<ShortcutId> { self.held().map(|(id, ..)| *id).collect() }

    #[cfg(test)]
    pub(super) fn host(&self, id: ShortcutId) -> Option<Option<HostId>> { self.held().find(|(held, ..)| **held == id).map(|(_, host, _)| host) }
}

impl<H: Clone> Handlers<H> {
    pub(super) fn handler(&self, id: ShortcutId) -> Option<H> { self.held().find(|(held, ..)| **held == id).map(|(.., handler)| handler.clone()) }
}

/// The table behind its lock, and the condition a close waits on until no handler is being dropped.
pub(super) struct Table<H> { table: Mutex<Handlers<H>>, dropped: Condvar }

impl<H> Default for Table<H> {
    fn default() -> Self { Self { table: Mutex::default(), dropped: Condvar::new() } }
}

impl<H> Table<H> {
    pub(super) fn lock(&self) -> LockResult<MutexGuard<'_, Handlers<H>>> { self.table.lock() }

    /// Drops a registration's handler, outside the lock.
    pub(super) fn remove(&self, id: ShortcutId) {
        let taken = self.lock().unwrap().take(id);
        self.drop_taken(taken.map(|handler| (id, handler)).into_iter().collect());
    }

    /// Waits until no handler is being dropped, as dev's lock made a close wait, then drops the
    /// handlers the close ends, outside the lock, and returns once they are gone.
    pub(super) fn close(&self, latest: HostId, released: &[ShortcutId]) {
        let mut table = self.dropped.wait_while(self.lock().unwrap(), |table| table.busy()).unwrap();
        let taken = table.close(latest, released);
        drop(table);
        self.drop_taken(taken);
    }

    fn drop_taken(&self, taken: Vec<(ShortcutId, H)>) {
        if taken.is_empty() { return; }
        let ids: Vec<ShortcutId> = taken.iter().map(|(id, _)| *id).collect();
        drop(taken);
        let mut table = self.lock().unwrap();
        for id in ids { table.dropped(id); }
        self.dropped.notify_all();
    }
}
