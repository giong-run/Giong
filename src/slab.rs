//! A minimal slab: stable `usize` keys, O(1) insert/remove, slots are reused.

use std::mem;

pub(crate) struct Slab<T> {
    entries: Vec<Entry<T>>,
    next: usize,
    len: usize,
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
        }
    }

    pub(crate) fn insert(&mut self, val: T) -> usize {
        let key = self.next;
        if key == self.entries.len() {
            self.entries.push(Entry::Occupied(val));
            self.next = key + 1;
        } else {
            match mem::replace(&mut self.entries[key], Entry::Occupied(val)) {
                Entry::Vacant(next) => self.next = next,
                Entry::Occupied(_) => unreachable!("slab free list points at an occupied slot"),
            }
        }
        self.len += 1;
        key
    }

    /// [`insert`](Self::insert), always inlined: for the spawn path, where
    /// the call costs more than the body. A copy, not a call from `insert`,
    /// so that the driver's calls keep the compiler's own inlining choice.
    #[inline(always)]
    pub(crate) fn insert_inline(&mut self, val: T) -> usize {
        let key = self.next;
        if key == self.entries.len() {
            self.entries.push(Entry::Occupied(val));
            self.next = key + 1;
        } else {
            match mem::replace(&mut self.entries[key], Entry::Occupied(val)) {
                Entry::Vacant(next) => self.next = next,
                Entry::Occupied(_) => unreachable!("slab free list points at an occupied slot"),
            }
        }
        self.len += 1;
        key
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
        /// once, and ends at `entries.len()`; every vacant slot is on it;
        /// `len` counts the occupied ones.
        fn well_formed(&self) -> bool {
            let n = self.entries.len();
            let occupied = self
                .entries
                .iter()
                .filter(|e| matches!(e, Entry::Occupied(_)))
                .count();
            if occupied != self.len {
                return false;
            }
            let mut at = self.next;
            let mut walked = 0;
            while at != n {
                match self.entries.get(at) {
                    Some(Entry::Vacant(next)) => at = *next,
                    _ => return false,
                }
                walked += 1;
                if walked > n {
                    return false; // a cycle
                }
            }
            // `walked` distinct vacant slots (a repeat would be a cycle).
            walked == n - occupied
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
        };
        kani::assume(slab.well_formed());
        slab
    }

    fn any_key() -> usize {
        let key: usize = kani::any();
        kani::assume(key <= SLOTS + 1);
        key
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn insert_takes_a_free_slot_and_keeps_the_rest() {
        let mut slab = any_slab();
        let n = slab.entries.len();
        let other = any_key();
        let before = slab.get(other).copied();
        let val: u8 = kani::any();
        // Never hits the `unreachable!`: the free list's head is vacant.
        let key = slab.insert(val);
        assert!(key <= n, "insert skipped a free slot");
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
    fn take_all_returns_every_value_and_empties() {
        let mut slab = any_slab();
        let len = slab.len();
        assert_eq!(slab.take_all().len(), len);
        assert!(slab.well_formed());
        assert_eq!(slab.insert(0), 0);
    }
}
