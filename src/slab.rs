//! A minimal slab: stable `usize` keys, O(1) insert/remove, slots are reused.

use std::cell::Cell;
use std::mem;
use std::ptr::NonNull;

/// A table smaller than this is never trimmed (see [`Slab::trim`]).
/// Smaller under Miri, so that the tests stay seconds long there.
const TRIM_MIN: usize = if cfg!(miri) { 64 } else { 4096 };

/// The most calls of [`Slab::trim`] skipped between two tries.
const TRIM_BACKOFF_MAX: u32 = if cfg!(miri) { 1 << 6 } else { 1 << 14 };

pub(crate) struct Slab<T> {
    entries: Vec<Entry<T>>,
    /// The head of the free list, which ends at `entries.len()`.
    next: usize,
    len: usize,
    /// Keys from here up are not handed out: `entries.len()`, except after
    /// a [`trim`](Slab::trim) that a value near the end kept from cutting
    /// the table. Then the slots past it are left to empty, for the next
    /// trim to cut off.
    end: usize,
    /// [`trim`](Slab::trim) is worth trying once `len` is below this; 0
    /// when it is not (see [`trim_due`](Slab::trim_due)).
    trim_below: usize,
    /// Calls of `trim` to skip before the next try, and how many were
    /// skipped before the last one (doubled after each try that leaves the
    /// table big).
    trim_wait: u32,
    trim_backoff: u32,
    /// A bit set in a flag when the table grows past [`TRIM_MIN`] (see
    /// [`watch_growth`](Slab::watch_growth)).
    on_grow: Option<(NonNull<Cell<u8>>, u8)>,
}

enum Entry<T> {
    Vacant(usize),
    Occupied(T),
}

impl<T> Slab<T> {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 0,
            len: 0,
            end: 0,
            trim_below: 0,
            trim_wait: 0,
            trim_backoff: 0,
            on_grow: None,
        }
    }

    /// Sets `bit` in `flag` whenever the table grows past a few thousand
    /// slots, so that an event loop reads one flag it reads anyway, not
    /// every table, before it sleeps (see [`trim`](Self::trim)).
    ///
    /// # Safety
    ///
    /// `flag` outlives the slab and stays where it is: both are fields of
    /// one value that does not move (a worker, in an `Rc`).
    pub(crate) unsafe fn watch_growth(&mut self, flag: &Cell<u8>, bit: u8) {
        self.on_grow = Some((NonNull::from(flag), bit));
    }

    pub(crate) fn insert(&mut self, val: T) -> usize {
        let mut key = self.next;
        if key >= self.end {
            key = self.make_room();
        }
        self.fill(key, val)
    }

    /// [`insert`](Self::insert), always inlined: for the spawn path, where
    /// the call costs more than the body. A copy, not a call from `insert`,
    /// so that the driver's calls keep the compiler's own inlining choice.
    #[inline(always)]
    pub(crate) fn insert_inline(&mut self, val: T) -> usize {
        let mut key = self.next;
        if key >= self.end {
            key = self.make_room();
        }
        self.fill(key, val)
    }

    /// Puts `val` in slot `key`, the free list's head, below `end` (as
    /// [`make_room`](Self::make_room) leaves it).
    #[inline(always)]
    fn fill(&mut self, key: usize, val: T) -> usize {
        debug_assert!(key == self.next && key < self.end);
        // SAFETY: `key` is below `end`, which is within the table
        // (`proofs` checks it). Unchecked, as the comparison with
        // `entries.len()` it replaces made it.
        let slot = unsafe { self.entries.get_unchecked_mut(key) };
        match mem::replace(slot, Entry::Occupied(val)) {
            Entry::Vacant(next) => self.next = next,
            Entry::Occupied(_) => unreachable!("slab free list points at an occupied slot"),
        }
        self.len += 1;
        key
    }

    /// The free list is empty, or its head is a key past `end` (freed by a
    /// value a trim could not cut off): such keys leave the list, for the
    /// next trim. Once the low keys are all taken, the held-back ones come
    /// back, and only then does the table grow, by a vacant slot. Returns
    /// the free list's head, now a vacant slot. Out of line, and without
    /// the value, so that `insert` builds the value in its slot.
    #[cold]
    #[inline(never)]
    fn make_room(&mut self) -> usize {
        let n = self.entries.len();
        while self.next != n && self.next >= self.end {
            match self.entries[self.next] {
                Entry::Vacant(next) => self.next = next,
                Entry::Occupied(_) => unreachable!("slab free list points at an occupied slot"),
            }
        }
        if self.next == n && self.end != n {
            self.link_vacant(n);
        }
        if self.next == n {
            self.entries.push(Entry::Vacant(n + 1));
            self.end = n + 1;
            self.grew();
        }
        self.next
    }

    /// Makes the free list every vacant slot below `keep`, lowest first;
    /// the keys from `keep` up are held back (see `end`).
    fn link_vacant(&mut self, keep: usize) {
        // A plain loop over indices: what Kani proves fastest.
        let mut next = self.entries.len();
        let mut key = keep;
        while key > 0 {
            key -= 1;
            if let Entry::Vacant(link) = &mut self.entries[key] {
                *link = next;
                next = key;
            }
        }
        self.next = next;
        self.end = keep;
    }

    /// The table grew by a slot (every other slot is taken): once three
    /// quarters of it are free again, [`trim`](Self::trim) may give them
    /// back. Only on growth, so `remove` and reuse pay nothing for it.
    fn grew(&mut self) {
        let n = self.entries.len();
        if n >= TRIM_MIN {
            self.trim_below = n / 4;
            self.trim_wait = 0;
            self.trim_backoff = 0;
            if let Some((flag, bit)) = self.on_grow {
                // SAFETY: the caller of `watch_growth` keeps the flag alive
                // and in place for as long as the slab.
                let flag = unsafe { flag.as_ref() };
                flag.set(flag.get() | bit);
            }
        }
    }

    /// Whether [`trim`](Self::trim) is worth calling: the table once grew
    /// past a few thousand slots and is now mostly vacant. One comparison,
    /// for an event loop to read before it sleeps.
    #[inline(always)]
    pub(crate) fn trim_due(&self) -> bool {
        self.len < self.trim_below
    }

    /// Whether the table is big enough for [`trim`](Self::trim) to be due
    /// one day: it grew past a few thousand slots and was not cut back.
    pub(crate) fn trim_armed(&self) -> bool {
        self.trim_below != 0
    }

    /// Gives back what a spike left behind: cuts the vacant slots at the
    /// end of the table and frees their memory, then rebuilds the free
    /// list so that the lowest vacant keys are handed out first (what is
    /// inserted from now on sits low, so the tail can go next time).
    ///
    /// Occupied slots never move: their keys are held elsewhere. While a
    /// value near the end keeps the table big, only the low keys are
    /// handed out (see `end`), so the slots past them empty as their
    /// values go, and the next tries wait for twice as many calls each
    /// time (up to [`TRIM_BACKOFF_MAX`]), so the O(n) pass is rare. Not for
    /// the hot path: an event loop calls it before it sleeps, when
    /// [`trim_due`](Self::trim_due) says so. Returns whether this call made
    /// the pass (a skipped call does nothing).
    #[cold]
    #[inline(never)]
    pub(crate) fn trim(&mut self) -> bool {
        if self.trim_wait > 0 {
            self.trim_wait -= 1;
            return false;
        }
        self.compact();
        let n = self.entries.len();
        if n < TRIM_MIN {
            self.trim_below = 0;
            self.trim_backoff = 0;
        } else {
            self.trim_below = n / 4;
            self.trim_backoff = self
                .trim_backoff
                .saturating_mul(2)
                .clamp(1, TRIM_BACKOFF_MAX);
            self.trim_wait = self.trim_backoff;
        }
        true
    }

    /// The work of [`trim`](Self::trim), whatever the table's size.
    fn compact(&mut self) {
        let end = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Occupied(_)))
            .map_or(0, |last| last + 1);
        self.entries.truncate(end);
        // Not under Kani: a reallocation of symbolic size runs CBMC out of
        // memory (see `proofs::any_slab`), and it moves no slot.
        #[cfg(not(kani))]
        self.entries.shrink_to(end);
        // Still big (a value near the end): hand out only the low keys, so
        // that the slots past them empty as their values go.
        let keep = if end >= TRIM_MIN {
            (self.len * 2).max(TRIM_MIN / 4).min(end)
        } else {
            end
        };
        self.link_vacant(keep);
    }

    /// The table's length in slots, occupied or not.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> usize {
        self.entries.capacity()
    }

    pub(crate) fn remove(&mut self, key: usize) -> Option<T> {
        match self.entries.get_mut(key) {
            Some(entry @ Entry::Occupied(_)) => {
                let old = mem::replace(entry, Entry::Vacant(self.next));
                self.next = key;
                self.len -= 1;
                match old {
                    Entry::Occupied(val) => Some(val),
                    Entry::Vacant(_) => unreachable!(),
                }
            }
            _ => None,
        }
    }

    pub(crate) fn get(&self, key: usize) -> Option<&T> {
        match self.entries.get(key) {
            Some(Entry::Occupied(val)) => Some(val),
            _ => None,
        }
    }

    pub(crate) fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        match self.entries.get_mut(key) {
            Some(Entry::Occupied(val)) => Some(val),
            _ => None,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().filter_map(|e| match e {
            Entry::Occupied(val) => Some(val),
            Entry::Vacant(_) => None,
        })
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.entries.iter_mut().filter_map(|e| match e {
            Entry::Occupied(val) => Some(val),
            Entry::Vacant(_) => None,
        })
    }

    /// Like [`iter_mut`](Self::iter_mut), with each value's key.
    #[cfg(target_os = "linux")]
    pub(crate) fn iter_keyed_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> {
        self.entries
            .iter_mut()
            .enumerate()
            .filter_map(|(key, e)| match e {
                Entry::Occupied(val) => Some((key, val)),
                Entry::Vacant(_) => None,
            })
    }

    /// Removes and returns every value, leaving the slab empty.
    pub(crate) fn take_all(&mut self) -> Vec<T> {
        let entries = mem::take(&mut self.entries);
        self.next = 0;
        self.len = 0;
        self.end = 0;
        self.trim_below = 0;
        self.trim_wait = 0;
        self.trim_backoff = 0;
        entries
            .into_iter()
            .filter_map(|e| match e {
                Entry::Occupied(val) => Some(val),
                Entry::Vacant(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Slab;

    #[test]
    fn reuse_and_take_all() {
        let mut s = Slab::new();
        let a = s.insert("a");
        let b = s.insert("b");
        let c = s.insert("c");
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(s.remove(b), Some("b"));
        assert_eq!(s.remove(b), None);
        assert_eq!(s.get(b), None);
        assert_eq!(s.insert("d"), b);
        assert_eq!(s.remove(a), Some("a"));
        assert_eq!(s.remove(c), Some("c"));
        assert_eq!(s.insert("e"), c);
        assert_eq!(s.insert("f"), a);
        assert_eq!(s.insert("g"), 3);
        assert_eq!(s.len(), 4);
        let mut all = s.take_all();
        all.sort();
        assert_eq!(all, vec!["d", "e", "f", "g"]);
        assert_eq!(s.len(), 0);
        assert_eq!(s.insert("h"), 0);
    }

    /// Tries until a call does the work (`trim` skips calls after a try
    /// that left the table big).
    fn trim_until_done<T>(s: &mut Slab<T>) {
        for _ in 0..=super::TRIM_BACKOFF_MAX {
            if s.trim_wait == 0 {
                break;
            }
            s.trim();
        }
        s.trim();
    }

    #[test]
    fn trim_gives_back_a_vacant_tail_and_hands_out_low_keys() {
        let n = super::TRIM_MIN * 2;
        let mut s = Slab::new();
        let keys: Vec<usize> = (0..n).map(|i| s.insert(i)).collect();
        assert!(!s.trim_due());
        // A survivor low and one in the middle; the rest drains in an
        // order that leaves the free list pointing high.
        for &k in &keys {
            if k != 3 && k != n / 2 {
                s.remove(k);
            }
        }
        assert_eq!(s.len(), 2);
        assert!(s.trim_due());
        s.trim();
        assert_eq!(s.slots(), n / 2 + 1, "the tail past the survivor is gone");
        // Still big and mostly vacant: tried again, after a wait.
        assert!(s.trim_due());
        assert_eq!(s.trim_wait, 1);
        // Lowest keys first, then the end of the table.
        assert_eq!(s.insert(0), 0);
        assert_eq!(s.insert(0), 1);
        assert_eq!(s.insert(0), 2);
        assert_eq!(s.insert(0), 4);
        for k in [0, 1, 2, 4] {
            s.remove(k);
        }
        // A try that cuts nothing doubles the wait.
        trim_until_done(&mut s);
        assert_eq!(s.slots(), n / 2 + 1);
        assert_eq!(s.trim_wait, 2);
        // The middle survivor goes and something takes its place at once
        // (a timer set again, a connection replaced): it gets a low key,
        // not the freed one, so the next try takes the table down to the
        // low survivor.
        assert_eq!(s.remove(n / 2), Some(n / 2));
        assert_eq!(s.insert(9), 0);
        assert_eq!(s.remove(0), Some(9));
        trim_until_done(&mut s);
        assert_eq!(s.slots(), 4);
        assert!(!s.trim_due(), "below the minimum, never again");
        assert_eq!(s.insert(0), 0);
        assert_eq!(s.insert(0), 1);
        assert_eq!(s.insert(0), 2);
        assert_eq!(s.insert(0), 4);
        assert_eq!(s.len(), 5);
        // Emptied, a trim leaves nothing.
        for k in [0, 1, 2, 3, 4] {
            s.remove(k);
        }
        s.trim();
        assert_eq!(s.slots(), 0);
        assert_eq!(s.insert(7), 0);
    }

    #[test]
    fn keys_held_back_after_a_trim_come_back_when_the_table_fills() {
        let n = super::TRIM_MIN * 2;
        let mut s = Slab::new();
        for i in 0..n {
            s.insert(i);
        }
        for k in 0..n - 1 {
            s.remove(k);
        }
        s.trim();
        // Only the low keys are handed out...
        let low = super::TRIM_MIN / 4;
        for i in 0..low {
            assert_eq!(s.insert(i), i);
        }
        // ...until they are all taken: then the held-back ones come back,
        // lowest first, before the table grows.
        let mut keys: Vec<usize> = (low..n - 1).map(|i| s.insert(i)).collect();
        assert_eq!(keys[0], low);
        keys.sort_unstable();
        assert_eq!(keys, (low..n - 1).collect::<Vec<_>>());
        assert_eq!(s.slots(), n);
        assert_eq!(s.insert(0), n);
    }

    #[test]
    fn a_table_that_grows_again_is_trimmed_again() {
        let n = super::TRIM_MIN;
        let mut s = Slab::new();
        for i in 0..n {
            s.insert(i);
        }
        for k in 0..n {
            s.remove(k);
        }
        assert!(s.trim_due());
        s.trim();
        assert_eq!(s.slots(), 0);
        assert!(!s.trim_due());
        for i in 0..n {
            assert_eq!(s.insert(i), i);
        }
        for k in 1..n {
            s.remove(k);
        }
        assert!(s.trim_due());
        s.trim();
        assert_eq!(s.slots(), 1);
    }

    #[test]
    fn the_wait_between_tries_is_bounded() {
        let n = super::TRIM_MIN;
        let mut s = Slab::new();
        for i in 0..n {
            s.insert(i);
        }
        for k in 0..n - 1 {
            s.remove(k);
        }
        // The last slot is held for good: tries keep failing.
        for _ in 0..16 {
            trim_until_done(&mut s);
        }
        assert_eq!(s.trim_backoff, super::TRIM_BACKOFF_MAX);
        assert_eq!(s.slots(), n);
        assert_eq!(s.insert(0), 0, "lowest key first");
        assert_eq!(s.insert(0), 1);
    }
}

/// Kani proofs (`scripts/check.sh kani`).
///
/// The proofs are inductive: each starts from *any* slab of up to `SLOTS`
/// slots that satisfies [`Slab::well_formed`], applies one operation with
/// any argument, and checks the result and that the slab is still well
/// formed. An empty slab is well formed, so together they cover every
/// sequence of operations, of any length, that never needs more than
/// `SLOTS` slots at once (the free list's walk is bounded by `SLOTS`).
#[cfg(kani)]
mod proofs {
    use super::{Entry, Slab};

    const SLOTS: usize = 5;

    impl<T> Slab<T> {
        /// The free list starts at `next`, visits only vacant slots, each
        /// once, and ends at `entries.len()`; `len` counts the occupied
        /// slots; `end` is within the table, and every vacant slot below it
        /// is on the list. A vacant slot from `end` up may be off the list:
        /// held back after a trim (see `end`).
        fn well_formed(&self) -> bool {
            let n = self.entries.len();
            if self.end > n {
                return false;
            }
            // One pass over the slots: CBMC's memory grows with each loop.
            let mut occupied = 0;
            let mut vacant_below_end = 0;
            for (key, entry) in self.entries.iter().enumerate() {
                match entry {
                    Entry::Occupied(_) => occupied += 1,
                    Entry::Vacant(_) => vacant_below_end += usize::from(key < self.end),
                }
            }
            if occupied != self.len {
                return false;
            }
            let mut at = self.next;
            let mut walked = 0;
            let mut walked_below_end = 0;
            while at != n {
                match self.entries.get(at) {
                    Some(Entry::Vacant(next)) => {
                        walked_below_end += usize::from(at < self.end);
                        at = *next;
                    }
                    _ => return false,
                }
                walked += 1;
                if walked > n {
                    return false; // a cycle
                }
            }
            // `walked` distinct vacant slots (a repeat would be a cycle).
            walked_below_end == vacant_below_end
        }
    }

    /// Any well-formed slab of up to `SLOTS` slots, with room for one more
    /// so that no operation reallocates (the standard library's code: a
    /// reallocation of symbolic size makes CBMC run out of memory).
    fn any_slab() -> Slab<u8> {
        let n: usize = kani::any();
        kani::assume(n <= SLOTS);
        let mut entries = Vec::with_capacity(SLOTS + 1);
        for _ in 0..n {
            entries.push(if kani::any() {
                Entry::Occupied(kani::any())
            } else {
                Entry::Vacant(kani::any())
            });
        }
        let slab = Slab {
            entries,
            next: kani::any(),
            len: kani::any(),
            end: kani::any(),
            trim_below: kani::any(),
            trim_wait: 0,
            trim_backoff: kani::any(),
            on_grow: None,
        };
        kani::assume(slab.well_formed());
        slab
    }

    fn any_key() -> usize {
        let key: usize = kani::any();
        kani::assume(key <= SLOTS + 1);
        key
    }

    /// `insert` is `make_room` when the free list's head is not below
    /// `end`, then `fill`: each is proven on its own (together, CBMC needed
    /// more memory than a CI runner has).
    #[kani::proof]
    #[kani::unwind(8)]
    fn insert_takes_a_free_slot_and_keeps_the_rest() {
        let mut slab = any_slab();
        kani::assume(slab.next < slab.end);
        let n = slab.entries.len();
        let other = any_key();
        let before = slab.get(other).copied();
        let val: u8 = kani::any();
        // Never hits the `unreachable!`: the free list's head is vacant.
        let key = slab.fill(slab.next, val);
        assert!(key < n, "insert skipped a free slot");
        assert_eq!(slab.get(key), Some(&val));
        if other != key {
            assert_eq!(slab.get(other).copied(), before);
        } else {
            assert!(before.is_none(), "insert reused an occupied key");
        }
        assert!(slab.well_formed());
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn make_room_finds_a_free_slot_or_grows() {
        let mut slab = any_slab();
        kani::assume(slab.next >= slab.end);
        let n = slab.entries.len();
        let len = slab.len();
        let other = any_key();
        let before = slab.get(other).copied();
        let had_vacant = slab.entries.iter().any(|e| matches!(e, Entry::Vacant(_)));
        let key = slab.make_room();
        // What `fill` takes: the head, vacant, below `end`.
        assert!(key == slab.next && key < slab.end);
        assert!(matches!(slab.entries.get(key), Some(Entry::Vacant(_))));
        // Held-back slots come back before the table grows, by one slot.
        if slab.entries.len() != n {
            assert!(!had_vacant, "the table grew with a slot free");
            assert!(slab.entries.len() == n + 1 && key == n);
        }
        assert_eq!(slab.get(other).copied(), before);
        assert_eq!(slab.len(), len);
        assert!(slab.well_formed());
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn remove_frees_exactly_its_slot() {
        let mut slab = any_slab();
        let key = any_key();
        let other = any_key();
        let had = slab.get(key).copied();
        let before = slab.get(other).copied();
        let len = slab.len();
        assert_eq!(slab.remove(key), had);
        assert_eq!(slab.get(key), None);
        if other != key {
            assert_eq!(slab.get(other).copied(), before);
        }
        assert_eq!(slab.len(), len - usize::from(had.is_some()));
        assert!(slab.well_formed());
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn trim_keeps_every_value_and_hands_out_the_lowest_key() {
        let mut slab = any_slab();
        let key = any_key();
        let before = slab.get(key).copied();
        let len = slab.len();
        slab.trim();
        assert_eq!(slab.get(key).copied(), before);
        assert_eq!(slab.len(), len);
        assert!(slab.well_formed());
        // The tail past the last value is gone, and the next insert takes
        // the lowest vacant key.
        let lowest = (0..slab.entries.len())
            .find(|&k| slab.get(k).is_none())
            .unwrap_or(slab.entries.len());
        assert!(slab
            .entries
            .last()
            .map_or(true, |e| matches!(e, Entry::Occupied(_))));
        assert_eq!(slab.insert(0), lowest);
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn take_all_returns_every_value_and_empties() {
        let mut slab = any_slab();
        let len = slab.len();
        assert_eq!(slab.take_all().len(), len);
        assert!(slab.well_formed());
        assert_eq!(slab.insert(0), 0);
    }
}
