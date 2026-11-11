//! Which host took each shortcut registration, so a close removes only the handlers it ends.

use super::super::ShortcutId;
use std::collections::HashMap;

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

/// Each registration's handler, with the host that took it: None until it is routed to one.
/// Every method that removes a handler hands it back, for the caller to drop after letting go
/// of the lock around this, since a handler's captures can run anything when they drop.
#[derive(Clone)]
pub(super) struct Handlers<H> {
    entries: HashMap<ShortcutId, (Option<HostId>, H)>,
}

impl<H> Default for Handlers<H> {
    fn default() -> Self { Self { entries: HashMap::new() } }
}

impl<H> Handlers<H> {
    /// A registration's handler, before any host has it. Returns what it replaced.
    #[must_use]
    pub(super) fn reserve(&mut self, id: ShortcutId, handler: H) -> Option<H> {
        self.entries.insert(id, (None, handler)).map(|(_, handler)| handler)
    }

    /// Routing handed the registration to `host`.
    pub(super) fn routed(&mut self, id: ShortcutId, host: HostId) {
        if let Some(entry) = self.entries.get_mut(&id) { entry.0 = Some(host); }
    }

    #[must_use]
    pub(super) fn remove(&mut self, id: ShortcutId) -> Option<H> {
        self.entries.remove(&id).map(|(_, handler)| handler)
    }

    /// A close that took routing when `latest` was the latest host, and let go of an engine
    /// holding `released`. It ends every host up to `latest`, and the registrations it released,
    /// which can be a later host's when that host's engine was the one it let go of. A later
    /// host keeps the rest, as does a registration no host has taken yet.
    #[must_use]
    pub(super) fn close(&mut self, latest: HostId, released: &[ShortcutId]) -> Vec<H> {
        let ended: Vec<ShortcutId> = self.entries.iter()
            .filter(|(id, (host, _))| host.is_some_and(|host| host <= latest) || released.contains(id))
            .map(|(id, _)| *id).collect();
        ended.into_iter().filter_map(|id| self.remove(id)).collect()
    }

    pub(super) fn contains_key(&self, id: &ShortcutId) -> bool { self.entries.contains_key(id) }

    pub(super) fn is_empty(&self) -> bool { self.entries.is_empty() }

    #[cfg(test)]
    pub(super) fn ids(&self) -> Vec<ShortcutId> { self.entries.keys().copied().collect() }

    #[cfg(test)]
    pub(super) fn host(&self, id: ShortcutId) -> Option<Option<HostId>> { self.entries.get(&id).map(|(host, _)| *host) }
}

impl<H: Clone> Handlers<H> {
    pub(super) fn handler(&self, id: ShortcutId) -> Option<H> { self.entries.get(&id).map(|(_, handler)| handler.clone()) }
}
