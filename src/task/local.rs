use std::any::Any;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use super::context::{self, Frame};
use crate::error::{Code, Error};

/// Declares task-local values: values that every call inside a piece of
/// work can read, set for that work with [`LocalKey::scope`].
///
/// ```
/// giong::task_local! {
///     /// The id of the request being served.
///     pub static REQUEST_ID: u64;
/// }
///
/// # giong::block_on(async {
/// REQUEST_ID
///     .scope(42, async {
///         assert_eq!(REQUEST_ID.get(), 42);
///         assert_eq!(REQUEST_ID.try_get(), Some(42));
///     })
///     .await;
/// assert_eq!(REQUEST_ID.try_get(), None);
/// # });
/// ```
///
/// The value is seen by everything the wrapped future polls, by the
/// children of a [`Scope`](super::Scope) inside it and by tasks started
/// there with [`TaskGroup::spawn`](super::TaskGroup::spawn). A task started
/// with plain [`spawn`](crate::spawn) does not inherit it.
#[macro_export]
macro_rules! task_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty; $($rest:tt)*) => {
        $(#[$attr])*
        $vis static $name: $crate::task::LocalKey<$t> =
            $crate::task::LocalKey::new(::core::stringify!($name));
        $crate::task_local!($($rest)*);
    };
}

/// A task-local value's key, declared with
/// [`task_local!`](crate::task_local).
pub struct LocalKey<T: 'static> {
    /// The static's name, for messages; it also gives each key its own
    /// address, which is how the key is told apart.
    name: &'static str,
    _value: PhantomData<fn() -> T>,
}

impl<T: 'static> LocalKey<T> {
    #[doc(hidden)]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _value: PhantomData,
        }
    }

    fn id(&'static self) -> usize {
        self as *const Self as usize
    }

    /// Runs `fut` with this key set to `value`. Inside, an outer value of
    /// the same key is hidden.
    pub fn scope<F: Future>(&'static self, value: T, fut: F) -> LocalScope<T, F> {
        LocalScope {
            key: self,
            value: Rc::new(value),
            fut,
        }
    }

    /// Runs `f` with this key set to `value`, for code that does not await.
    pub fn sync_scope<R>(&'static self, value: T, f: impl FnOnce() -> R) -> R {
        let value: Rc<dyn Any> = Rc::new(value);
        context::with_frame(
            Frame::One {
                key: self.id(),
                value: &value,
            },
            f,
        )
    }

    /// Lends the value to `f`, or `None` where the key is not set.
    pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Option<R> {
        context::find_local(self.id(), |value| {
            value.map(|v| {
                f(v.downcast_ref::<T>()
                    .expect("a key holds values of its type"))
            })
        })
    }

    /// Lends the value to `f`.
    ///
    /// # Panics
    ///
    /// With [`GIONG-E0022`](crate::error::Code::TaskLocalNotSet) where the
    /// key is not set: outside of its [`scope`](Self::scope), or in a task
    /// started with plain [`spawn`](crate::spawn).
    #[track_caller]
    pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        match self.try_with(f) {
            Some(r) => r,
            None => not_set(self.name),
        }
    }

    /// A copy of the value, or `None` where the key is not set.
    pub fn try_get(&'static self) -> Option<T>
    where
        T: Clone,
    {
        self.try_with(T::clone)
    }

    /// A copy of the value.
    ///
    /// # Panics
    ///
    /// As [`with`](Self::with), where the key is not set.
    #[track_caller]
    pub fn get(&'static self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }
}

impl<T: 'static> fmt::Debug for LocalKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKey")
            .field("name", &self.name)
            .finish()
    }
}

#[cold]
#[track_caller]
fn not_set(name: &str) -> ! {
    panic!(
        "{}",
        Error::new(
            Code::TaskLocalNotSet,
            format!("the task-local value {name} is read where it is not set"),
            format!(
                "read it inside {name}.scope(value, future), or use try_with/try_get where it \
                 may be missing. Tasks started with giong::spawn do not inherit task-local \
                 values; start them with a giong::task::Scope or TaskGroup to pass them on."
            ),
        )
    )
}

/// Future returned by [`LocalKey::scope`].
#[must_use = "futures do nothing unless awaited"]
pub struct LocalScope<T: 'static, F> {
    key: &'static LocalKey<T>,
    value: Rc<dyn Any>,
    /// Pinned with the `LocalScope`: never moved out.
    fut: F,
}

impl<T: 'static, F: Future> Future for LocalScope<T, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `fut` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved.
        let this = unsafe { self.get_unchecked_mut() };
        let frame = Frame::One {
            key: this.key.id(),
            value: &this.value,
        };
        // SAFETY: `fut` is structurally pinned (see above).
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        context::with_frame(frame, || fut.poll(cx))
    }
}

impl<T: 'static, F> fmt::Debug for LocalScope<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalScope")
            .field("key", &self.key.name)
            .finish_non_exhaustive()
    }
}
