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
