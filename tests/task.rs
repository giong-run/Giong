//! Structured concurrency: deadlines, scopes, task groups, task-local values.

use std::cell::{Cell, RefCell};
use std::future::{pending, Future};
use std::io::{self, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::error::{self, Code};
use giong::net::{TcpListener, TcpStream};
use giong::task::{self, CancelReason, Cancelled, Scope, TaskGroup};
use giong::time::sleep;

mod common;

/// Sets a flag when dropped: shows that a future was cancelled.
struct DropFlag(Rc<Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

// ---------------------------------------------------------------- deadlines

/// `fut` under a deadline `d` from now: a scope with a deadline and no
/// children, with `Cancelled` as its error type.
async fn within<T>(d: Duration, fut: impl Future<Output = T>) -> Result<T, Cancelled> {
    Scope::<Cancelled>::with_timeout(d)
        .run(async { Ok(fut.await) })
        .await
}

/// `fut` under the deadline `at`.
async fn until<T>(at: Instant, fut: impl Future<Output = T>) -> Result<T, Cancelled> {
    Scope::<Cancelled>::with_deadline(at)
        .run(async { Ok(fut.await) })
        .await
}

/// The way an application writes it: `io::Error`, `?` on the deadline.
#[test]
fn a_scope_with_a_deadline_returns_a_coded_io_error() {
    giong::block_on(async {
        let out: io::Result<()> = Scope::with_timeout(Duration::from_millis(10))
            .run(async {
                sleep(Duration::from_secs(5)).await;
                Ok(())
            })
            .await;
        let err = out.unwrap_err();
        assert_eq!(error::code(&err), Some(Code::DeadlineExceeded));
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    });
}

#[test]
fn a_deadline_ends_the_work_and_says_why() {
    giong::block_on(async {
        let dropped = Rc::new(Cell::new(false));
        let flag = DropFlag(dropped.clone());
        let start = Instant::now();
        let out = within(Duration::from_millis(30), async move {
            let _flag = flag;
            pending::<()>().await
        })
        .await;
        let elapsed = start.elapsed();
        let err = out.unwrap_err();
        assert_eq!(err.reason(), CancelReason::DeadlineExceeded);
        assert!(err.is_deadline_exceeded());
        assert!(dropped.get(), "the work was not dropped");
        assert!(elapsed >= Duration::from_millis(30), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
        let io: io::Error = err.into();
        assert_eq!(error::code(&io), Some(Code::DeadlineExceeded));
        assert_eq!(io.kind(), io::ErrorKind::TimedOut);
    });
}

#[test]
fn work_that_finishes_in_time_returns_its_value() {
    giong::block_on(async {
        let out = within(Duration::from_secs(5), async { 7 }).await;
        assert_eq!(out, Ok(7));
    });
}

#[test]
fn the_deadline_is_readable_inside_and_the_earlier_one_wins() {
    giong::block_on(async {
        assert_eq!(task::deadline(), None);
        assert_eq!(task::remaining(), None);
        let outer = Instant::now() + Duration::from_secs(1);
        until(outer, async {
            assert_eq!(task::deadline(), Some(outer));
            // A later inner deadline cannot extend the outer one.
            within(Duration::from_secs(60), async {
                assert_eq!(task::deadline(), Some(outer));
            })
            .await
            .unwrap();
            // An earlier one takes over inside, and only inside.
            let inner = Instant::now() + Duration::from_millis(100);
            until(inner, async {
                assert_eq!(task::deadline(), Some(inner));
                assert!(task::remaining().unwrap() <= Duration::from_millis(100));
            })
            .await
            .unwrap();
            assert_eq!(task::deadline(), Some(outer));
        })
        .await
        .unwrap();
        assert_eq!(task::deadline(), None);
    });
}

#[test]
fn an_inner_deadline_ends_only_its_own_work() {
    giong::block_on(async {
        let out = within(Duration::from_secs(5), async {
            let inner = within(Duration::from_millis(10), pending::<()>()).await;
            assert!(inner.is_err());
            "outer goes on"
        })
        .await;
        assert_eq!(out, Ok("outer goes on"));
    });
}

#[test]
fn a_plain_spawn_does_not_inherit_the_deadline() {
    giong::block_on(async {
        let mut handle = None;
        within(Duration::from_secs(5), async {
            handle = Some(giong::spawn(async { task::deadline() }));
        })
        .await
        .unwrap();
        assert_eq!(handle.unwrap().await.unwrap(), None);
    });
}

/// Code that retries a failing call in a loop still ends at the deadline:
/// an operation started past the deadline waits instead of failing at once,
/// so the loop yields and the deadline gets to end it.
#[test]
#[cfg_attr(miri, ignore = "Miri has no SO_REUSEPORT or TCP connect")]
fn a_retry_loop_ends_at_the_deadline() {
    let (_refusing, addr) = common::refusing_addr();
    giong::block_on(async move {
        let start = Instant::now();
        let attempts = Rc::new(Cell::new(0u32));
        let counter = attempts.clone();
        let out = within(Duration::from_millis(50), async move {
            loop {
                counter.set(counter.get() + 1);
                if TcpStream::connect(addr).await.is_ok() {
                    return;
                }
            }
        })
        .await;
        assert!(out.is_err());
        assert!(attempts.get() > 0);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    });
}

/// The deadline reaches the kernel: a one-off read that is still waiting at
/// the deadline is cancelled there, even while the worker is busy, so data
/// that arrives later stays in the socket for the next reader. Without the
/// linked timeout the read would complete in the kernel and take the data
/// into a buffer that the dropped work never reads.
#[test]
#[cfg_attr(miri, ignore = "Miri has no SO_REUSEPORT or TCP connect")]
fn a_read_waiting_at_the_deadline_is_cancelled_by_the_kernel() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        // Well after the deadline, while the worker is still blocked.
        std::thread::sleep(Duration::from_millis(150));
        conn.write_all(b"late").unwrap();
        let mut buf = [0u8; 1];
        let _ = conn.read(&mut buf);
    });
    let (got, choice) = giong::block_on(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let stream = Rc::new(stream);
        let reader = stream.clone();
        let blocker = giong::spawn(async {
            sleep(Duration::from_millis(5)).await;
            // Keeps the worker from running anything, past the deadline
            // and past the moment the data arrives.
            std::thread::sleep(Duration::from_millis(250));
        });
        let out = within(Duration::from_millis(40), async move {
            reader.read(vec![0u8; 16]).await.0
        })
        .await;
        assert!(out.is_err(), "{out:?}");
        blocker.await.unwrap();
        // Bounded: a read that lost the data waits for ever otherwise.
        let got =
            match giong::time::timeout(Duration::from_secs(1), stream.read(vec![0u8; 16])).await {
                Ok((n, buf)) => {
                    let n = n.unwrap();
                    stream.write_all(&b"x"[..]).await.0.unwrap();
                    Some(buf[..n].to_vec())
                }
                Err(_) => None,
            };
        (got, giong::driver_choice())
    });
    peer.join().unwrap();
    let linked = choice
        .io_uring_features
        .contains(&giong::IoUringFeature::LinkedDeadlines);
    match choice.kind {
        // epoll reads only when the worker asks, which it no longer does
        // once the deadline has passed: the data stays.
        giong::DriverKind::Epoll => assert_eq!(got.as_deref(), Some(&b"late"[..])),
        _ if linked => assert_eq!(got.as_deref(), Some(&b"late"[..])),
        // io_uring without linked deadlines (GIONG_IO_URING_OFF): the read
        // completed in the kernel while the worker was busy, and the data
        // went with the dropped work. This is what the link prevents.
        _ => assert_eq!(got, None),
    }
}

/// Many operations under deadlines, some finishing in time and some cut,
/// leave the worker able to go on, with every connection usable.
#[test]
#[cfg_attr(miri, ignore = "Miri has no SO_REUSEPORT or TCP connect")]
fn deadlines_leave_nothing_behind() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Echoes on odd connections, stays silent on even ones.
        let peer = giong::spawn(async move {
            let mut conns = Vec::new();
            for j in 0..20 {
                let conn = Rc::new(listener.accept().await.unwrap());
                if j % 2 == 1 {
                    let conn = conn.clone();
                    giong::spawn(async move {
                        let (n, buf) = conn.read(vec![0u8; 8]).await;
                        let n = n.unwrap();
                        conn.write_all(buf[..n].to_vec()).await.0.unwrap();
                    });
                }
                conns.push(conn);
            }
            conns
        });
        let mut cut = 0;
        for i in 0..20 {
            let stream = TcpStream::connect(addr).await.unwrap();
            let out = within(Duration::from_millis(100), async {
                stream.write_all(&b"x"[..]).await.0.unwrap();
                stream.read(vec![0u8; 8]).await.0
            })
            .await;
            match out {
                Ok(read) => {
                    assert_eq!(i % 2, 1);
                    assert_eq!(read.unwrap(), 1);
                }
                Err(_) => {
                    assert_eq!(i % 2, 0);
                    cut += 1;
                }
            }
        }
        assert_eq!(cut, 10);
        drop(peer.await.unwrap());
    });
}

// ------------------------------------------------------------------- scopes

/// A scope's own error type: anything that converts from `Cancelled`.
#[derive(Debug, PartialEq)]
struct Failure(&'static str);

impl From<Cancelled> for Failure {
    fn from(_: Cancelled) -> Self {
        Failure("cancelled")
    }
}

#[test]
fn children_borrow_local_data_and_return_their_values() {
    giong::block_on(async {
        let words = ["giong".to_string(), "runtime".to_string()];
        let scope = Scope::new();
        let (a, b) = scope
            .run(async {
                let a = scope.spawn(async { Ok(words[0].len()) });
                let b = scope.spawn(async {
                    sleep(Duration::from_millis(5)).await;
                    Ok(words[1].len())
                });
                Ok::<_, io::Error>((a.await, b.await))
            })
            .await
            .unwrap();
        assert_eq!((a, b), (5, 7));
    });
}

#[test]
fn the_run_waits_for_every_child() {
    giong::block_on(async {
        let finished = Cell::new(0);
        let scope = Scope::new();
        scope
            .run(async {
                for i in 0..10u64 {
                    let finished = &finished;
                    drop(scope.spawn(async move {
                        sleep(Duration::from_millis(i * 2)).await;
                        finished.set(finished.get() + 1);
                        Ok(())
                    }));
                }
                Ok::<_, io::Error>(())
            })
            .await
            .unwrap();
        assert_eq!(finished.get(), 10);
    });
}

#[test]
fn the_first_error_cancels_the_other_children_and_the_body() {
    giong::block_on(async {
        let sibling_dropped = Rc::new(Cell::new(false));
        let body_dropped = Rc::new(Cell::new(false));
        let scope: Scope<'_, Failure> = Scope::new();
        let flag = DropFlag(sibling_dropped.clone());
        let body_flag = DropFlag(body_dropped.clone());
        let out: Result<(), Failure> = scope
            .run(async {
                let _body_flag = body_flag;
                drop(scope.spawn(async move {
                    let _flag = flag;
                    pending::<Result<(), Failure>>().await
                }));
                drop(scope.spawn(async {
                    sleep(Duration::from_millis(5)).await;
                    Err::<(), _>(Failure("first"))
                }));
                drop(scope.spawn(async {
                    sleep(Duration::from_millis(50)).await;
                    Err::<(), _>(Failure("second"))
                }));
                pending::<()>().await;
                Ok(())
            })
            .await;
        assert_eq!(out, Err(Failure("first")));
        assert!(sibling_dropped.get());
        assert!(body_dropped.get());
    });
}

#[test]
fn a_body_error_cancels_the_children() {
    giong::block_on(async {
        let dropped = Rc::new(Cell::new(false));
        let flag = DropFlag(dropped.clone());
        let scope = Scope::new();
        let out: io::Result<()> = scope
            .run(async {
                drop(scope.spawn(async move {
                    let _flag = flag;
                    pending::<io::Result<()>>().await
                }));
                sleep(Duration::from_millis(5)).await;
                Err(io::Error::other("body failed"))
            })
            .await;
        assert_eq!(out.unwrap_err().to_string(), "body failed");
        assert!(dropped.get());
    });
}

#[test]
fn cancel_stops_the_children_and_keeps_a_finished_body_value() {
    giong::block_on(async {
        // The body returned: its value stands, the endless child goes.
        let scope = Scope::new();
        let out = scope
            .run(async {
                drop(scope.spawn(pending::<io::Result<()>>()));
                scope.cancel();
                Ok::<_, io::Error>("done")
            })
            .await;
        assert_eq!(out.unwrap(), "done");

        // The body had not returned: the scope reports the cancel.
        let scope = Scope::new();
        let handle = scope.cancel_handle();
        let out: io::Result<()> = scope
            .run(async {
                let canceller = scope.spawn(async move {
                    sleep(Duration::from_millis(5)).await;
                    handle.cancel();
                    Ok(())
                });
                canceller.await;
                pending::<()>().await;
                Ok(())
            })
            .await;
        let err = out.unwrap_err();
        assert_eq!(error::code(&err), Some(Code::ScopeCancelled));
    });
}

#[test]
fn a_scope_deadline_ends_the_run_and_children_inherit_it() {
    giong::block_on(async {
        let seen = Cell::new(None);
        // Long enough for the child to run first, even under valgrind.
        let scope = Scope::with_timeout(Duration::from_millis(200));
        let start = Instant::now();
        let out: io::Result<()> = scope
            .run(async {
                drop(scope.spawn(async {
                    seen.set(task::deadline());
                    pending::<io::Result<()>>().await
                }));
                Ok(())
            })
            .await;
        let err = out.unwrap_err();
        assert_eq!(error::code(&err), Some(Code::DeadlineExceeded));
        assert!(seen.get().is_some());
        assert!(start.elapsed() < Duration::from_secs(2));
    });
}

#[test]
fn children_fan_out_through_scopes_of_their_own() {
    giong::block_on(async {
        let total = Cell::new(0);
        let scope = Scope::new();
        scope
            .run(async {
                let total = &total;
                drop(scope.spawn(async move {
                    let inner = Scope::new();
                    inner
                        .run(async {
                            for i in 1..=3 {
                                drop(inner.spawn(async move {
                                    giong::yield_now().await;
                                    total.set(total.get() + i);
                                    Ok(())
                                }));
                            }
                            Ok::<_, io::Error>(())
                        })
                        .await
                }));
                // The body can start children at any point.
                drop(scope.spawn(async move {
                    total.set(total.get() + 10);
                    Ok(())
                }));
                Ok::<_, io::Error>(())
            })
            .await
            .unwrap();
        assert_eq!(total.get(), 16);
    });
}

#[test]
fn a_child_that_keeps_yielding_does_not_starve_the_others() {
    giong::block_on(async {
        let stop = Cell::new(false);
        let scope = Scope::new();
        scope
            .run(async {
                let stop = &stop;
                drop(scope.spawn(async move {
                    while !stop.get() {
                        giong::yield_now().await;
                    }
                    Ok(())
                }));
                drop(scope.spawn(async move {
                    sleep(Duration::from_millis(5)).await;
                    stop.set(true);
                    Ok(())
                }));
                Ok::<_, io::Error>(())
            })
            .await
            .unwrap();
    });
}

#[test]
fn a_panicking_child_unwinds_out_of_the_run() {
    let payload = giong::block_on(async {
        giong::spawn(async {
            let scope = Scope::new();
            let _ = scope
                .run(async {
                    drop(scope.spawn(async {
                        giong::yield_now().await;
                        panic!("child broke");
                        #[allow(unreachable_code)]
                        Ok::<(), io::Error>(())
                    }));
                    pending::<io::Result<()>>().await
                })
                .await;
        })
        .await
        .unwrap_err()
        .try_into_panic()
        .unwrap()
    });
    assert_eq!(panic_message(payload), "child broke");
}

#[test]
fn a_scope_runs_one_run_at_a_time() {
    let payload = giong::block_on(async {
        giong::spawn(async {
            let scope: Scope<'_> = Scope::new();
            let first = scope.run(pending::<io::Result<()>>());
            let second = scope.run(async { Ok(()) });
            let mut first = Box::pin(first);
            let mut second = Box::pin(second);
            std::future::poll_fn(|cx| {
                let _ = first.as_mut().poll(cx);
                let _ = second.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;
        })
        .await
        .unwrap_err()
        .try_into_panic()
        .unwrap()
    });
    assert!(panic_message(payload).starts_with("GIONG-E0026"));
}

#[test]
fn dropping_the_run_drops_the_children() {
    giong::block_on(async {
        let dropped = Rc::new(Cell::new(false));
        let flag = DropFlag(dropped.clone());
        let scope = Scope::new();
        let out = giong::time::timeout(
            Duration::from_millis(10),
            scope.run(async {
                drop(scope.spawn(async move {
                    let _flag = flag;
                    pending::<io::Result<()>>().await
                }));
                pending::<io::Result<()>>().await
            }),
        )
        .await;
        assert!(out.is_err());
        assert!(dropped.get());
    });
}

// -------------------------------------------------------------- task groups

#[test]
fn a_group_returns_every_result() {
    giong::block_on(async {
        let mut group = TaskGroup::new();
        for i in 0..20u64 {
            group.spawn(async move {
                sleep(Duration::from_millis(20 - i)).await;
                i
            });
        }
        assert_eq!(group.len(), 20);
        let mut got = Vec::new();
        while let Some(result) = group.join_next().await {
            got.push(result.unwrap());
        }
        got.sort_unstable();
        assert_eq!(got, (0..20).collect::<Vec<_>>());
        assert!(group.is_empty());
        assert!(group.join_next().await.is_none());
    });
}

#[test]
fn dropping_a_group_aborts_its_tasks() {
    giong::block_on(async {
        let dropped = Rc::new(Cell::new(false));
        let flag = DropFlag(dropped.clone());
        let mut group = TaskGroup::new();
        group.spawn(async move {
            let _flag = flag;
            pending::<()>().await
        });
        giong::yield_now().await;
        drop(group);
        giong::yield_now().await;
        assert!(dropped.get());
    });
}

#[test]
fn a_group_reports_panics_and_aborts() {
    giong::block_on(async {
        let mut group = TaskGroup::new();
        group.spawn(async { panic!("task broke") });
        let err = group.join_next().await.unwrap().unwrap_err();
        assert!(err.is_panic());
        group.spawn(pending::<()>());
        group.abort_all();
        let err = group.join_next().await.unwrap().unwrap_err();
        assert!(err.is_cancelled());
        assert!(!err.is_deadline_exceeded());
    });
}

#[test]
fn group_tasks_inherit_the_deadline_where_they_were_started() {
    giong::block_on(async {
        let mut group = TaskGroup::new();
        // The group outlives the deadline its task was started under.
        within(Duration::from_millis(20), async {
            group.spawn(async {
                assert!(task::deadline().is_some());
                pending::<()>().await
            });
        })
        .await
        .unwrap();
        let err = group.join_next().await.unwrap().unwrap_err();
        assert!(err.is_deadline_exceeded());
        assert!(err.is_cancelled());
    });
}

// ---------------------------------------------------------- task-local values

giong::task_local! {
    static REQUEST_ID: u64;
    static TENANT: String;
}

#[test]
fn task_locals_are_set_for_their_work_only() {
    giong::block_on(async {
        assert_eq!(REQUEST_ID.try_get(), None);
        REQUEST_ID
            .scope(1, async {
                assert_eq!(REQUEST_ID.get(), 1);
                giong::yield_now().await;
                assert_eq!(REQUEST_ID.get(), 1);
                // Shadowed inside, back outside.
                REQUEST_ID
                    .scope(2, async { assert_eq!(REQUEST_ID.get(), 2) })
                    .await;
                assert_eq!(REQUEST_ID.get(), 1);
                assert_eq!(TENANT.try_get(), None);
                TENANT
                    .scope("acme".to_string(), async {
                        TENANT.with(|t| assert_eq!(t, "acme"));
                        assert_eq!(REQUEST_ID.get(), 1);
                    })
                    .await;
            })
            .await;
        assert_eq!(REQUEST_ID.try_get(), None);
        assert_eq!(REQUEST_ID.sync_scope(9, || REQUEST_ID.get()), 9);
    });
}

#[test]
fn concurrent_tasks_each_see_their_own_value() {
    giong::block_on(async {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut handles = Vec::new();
        for id in 0..5u64 {
            let seen = seen.clone();
            handles.push(giong::spawn(REQUEST_ID.scope(id, async move {
                for _ in 0..3 {
                    giong::yield_now().await;
                    seen.borrow_mut().push((id, REQUEST_ID.get()));
                }
            })));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(seen.borrow().iter().all(|(id, got)| id == got));
        assert_eq!(seen.borrow().len(), 15);
    });
}

#[test]
fn scopes_and_groups_pass_task_locals_on_and_spawn_does_not() {
    giong::block_on(async {
        REQUEST_ID
            .scope(42, async {
                let scope = Scope::new();
                let in_child = scope
                    .run(async {
                        let child = scope.spawn(async {
                            giong::yield_now().await;
                            Ok(REQUEST_ID.get())
                        });
                        Ok::<_, io::Error>(child.await)
                    })
                    .await
                    .unwrap();
                assert_eq!(in_child, 42);

                let mut group = TaskGroup::new();
                group.spawn(async {
                    giong::yield_now().await;
                    REQUEST_ID.get()
                });
                assert_eq!(group.join_next().await.unwrap().unwrap(), 42);

                let detached = giong::spawn(async { REQUEST_ID.try_get() });
                assert_eq!(detached.await.unwrap(), None);
            })
            .await;
    });
}

#[test]
fn reading_an_unset_task_local_panics_naming_the_key() {
    let payload = catch_unwind(AssertUnwindSafe(|| REQUEST_ID.get())).unwrap_err();
    let message = panic_message(payload);
    assert!(message.contains("REQUEST_ID"), "{message}");
}

#[test]
fn cancelled_converts_to_coded_io_errors() {
    let _ = Cancelled::clone;
    giong::block_on(async {
        let err = within(Duration::ZERO, pending::<()>()).await.unwrap_err();
        assert_eq!(err.to_string(), "the deadline passed");
        let io: io::Error = err.into();
        assert!(io.to_string().starts_with("GIONG-E0024"));
    });
}

/// A child woken from another thread (a channel fed by a plain thread) is
/// polled again: scope wakers work across threads.
#[test]
fn a_child_woken_from_another_thread_runs() {
    giong::block_on(async {
        let (tx, rx) = giong::sync::oneshot::channel::<u32>();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            tx.send(5).unwrap();
        });
        let scope = Scope::new();
        let got = scope
            .run(async {
                let child = scope.spawn(async { Ok(rx.await.unwrap()) });
                Ok::<_, io::Error>(child.await)
            })
            .await
            .unwrap();
        assert_eq!(got, 5);
        sender.join().unwrap();
    });
}
