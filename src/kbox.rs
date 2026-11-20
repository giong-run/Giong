//! Heap memory the kernel reads or writes while an operation is in flight.

use std::ffi::{c_char, CString};
use std::ptr::NonNull;

/// A heap value reached only through a raw pointer, from its allocation to
/// its drop.
///
/// An operation hands the kernel (or, on the epoll driver, a blocking
/// thread) pointers into memory that its resources own, and the resources
/// then move: into the [`Op`](crate::op::Op), into the driver if the future
/// is dropped, and back out. Moving a `Box` asserts that the `Box` is the
/// only way to reach its allocation, so a write through a pointer taken
/// before the move, followed by any use of the `Box`, is undefined
/// behaviour under Rust's aliasing rules (Stacked and Tree Borrows both say
/// so, and Miri reports it). A raw pointer asserts nothing when it moves,
/// so pointers taken from a `KernelBox` stay valid wherever it goes.
///
/// Read the value with [`get`](Self::get) only while no operation uses it:
/// once the `Op` has handed its resources back.
pub(crate) struct KernelBox<T>(NonNull<T>);

impl<T> KernelBox<T> {
    pub(crate) fn new(value: T) -> Self {
        Self::from_box(Box::new(value))
    }

    pub(crate) fn from_box(value: Box<T>) -> Self {
        // SAFETY: `Box::into_raw` never returns null.
        Self(unsafe { NonNull::new_unchecked(Box::into_raw(value)) })
    }

    /// The pointer to give the kernel. Valid until `self` is dropped.
    pub(crate) fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    /// The value, for reading once the kernel is done with it.
    pub(crate) fn get(&self) -> &T {
        // SAFETY: the allocation lives as long as `self`; the caller reads
        // only while no operation writes to it (see the type's docs).
        unsafe { self.0.as_ref() }
    }
}

impl KernelBox<CString> {
    /// A path for the kernel, as a C string.
    pub(crate) fn path(path: CString) -> Self {
        Self::new(path)
    }

    /// The pointer to the path's bytes, valid until `self` is dropped. The
    /// `CString` itself never moves, so neither does the buffer it owns.
    pub(crate) fn c_ptr(&self) -> *const c_char {
        self.get().as_ptr()
    }
}

impl<T> Drop for KernelBox<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::into_raw` and is dropped once.
        // An operation that might still use the memory owns `self` until it
        // is done (the driver keeps the resources of a dropped future).
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

#[cfg(test)]
mod tests {
    use super::KernelBox;

    /// Writing through the pointer after the box moved is fine (run under
    /// Miri, this is what a `Box` gets wrong).
    #[test]
    fn writes_through_its_pointer_survive_moves() {
        let kb = KernelBox::new([0u8; 16]);
        let ptr = kb.as_ptr();
        let moved = (kb, 1);
        // SAFETY: `ptr` points at the live allocation `moved` owns.
        unsafe { (*ptr)[3] = 7 };
        let kb = moved.0;
        assert_eq!(kb.get()[3], 7);

        let path = KernelBox::path(std::ffi::CString::new("/tmp/x").unwrap());
        let p = path.c_ptr();
        let moved = (path, 1);
        // SAFETY: the C string is alive and NUL-terminated.
        let read = unsafe { std::ffi::CStr::from_ptr(p) };
        assert_eq!(read.to_bytes(), b"/tmp/x");
        drop(moved);
    }
}

/// Kani proofs (`scripts/check.sh kani`): what an operation does with a
/// `KernelBox` (take the pointer, move the box into the operation and on
/// to the driver, have the "kernel" write through the pointer, read the
/// value back, drop it) touches only live memory, and the drop frees the
/// allocation exactly once. Kani's memory model reports a write to freed
/// memory, a double free and a dangling read as failures. Kani does not
/// model the aliasing rules (Stacked or Tree Borrows); the Miri job in CI
/// covers those for the same type.
#[cfg(kani)]
mod proofs {
    use super::KernelBox;

    #[kani::proof]
    fn pointer_survives_moves_and_drop_frees_once() {
        let first: u64 = kani::any();
        let written: u64 = kani::any();
        let kb = KernelBox::new(first);
        let ptr = kb.as_ptr();
        assert_eq!(*kb.get(), first);
        // Into an operation, then into the driver as `Box<dyn Any>`, as a
        // dropped future's resources go.
        let op = (kb, 0u8);
        let parked: Box<dyn std::any::Any> = Box::new(op);
        // SAFETY: `ptr` points at the allocation `parked` owns.
        unsafe { ptr.write(written) };
        let (kb, _) = *parked.downcast::<(KernelBox<u64>, u8)>().unwrap();
        assert_eq!(kb.as_ptr(), ptr);
        assert_eq!(*kb.get(), written);
        drop(kb);
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn path_pointer_stays_valid() {
        let byte: u8 = kani::any();
        kani::assume(byte != 0);
        let path = KernelBox::path(std::ffi::CString::new([byte, byte, byte]).unwrap());
        let p = path.c_ptr();
        let moved = Box::new(path);
        // Read it as the kernel does, byte by byte up to the NUL (Kani has
        // no `strlen`, which `CStr::from_ptr` calls).
        for i in 0..4 {
            // SAFETY: the C string `moved` owns is alive, 3 bytes and a NUL.
            let c = unsafe { p.add(i).read() } as u8;
            assert_eq!(c, if i < 3 { byte } else { 0 });
        }
        drop(moved);
    }
}
