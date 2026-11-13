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
