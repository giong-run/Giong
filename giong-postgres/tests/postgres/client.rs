//! Statements on one connection: types, errors, pipelining, the statement
//! cache, limits, cancellation and transactions.

use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use giong::time::{sleep, timeout};
use giong_postgres::{Client, ErrorKind};

use crate::run;
use crate::server::Server;

async fn connect(s: &Server) -> Client {
    Client::connect(&s.local()).await.unwrap()
}

/// A table only this test uses.
async fn table(client: &Client, name: &str, columns: &str) {
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {name}; CREATE TABLE {name} ({columns})"
        ))
        .await
        .unwrap();
}

async fn count(client: &Client, table: &str) -> i64 {
    let row = client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap();
    row.get(0).unwrap()
}

/// Waits until every task and operation but the caller's is gone.
async fn assert_no_leaks() {
    for _ in 0..200 {
        let s = giong::stats();
        if s.tasks == 0 && s.tracked_ops == 0 && s.free_recv_buffers == s.recv_buffers {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("leaked: {:?}", giong::stats());
}

#[test]
fn values_round_trip_through_every_supported_type() {
    run(|s| async move {
        let client = connect(s).await;
        let now = UNIX_EPOCH + Duration::from_micros(1_767_225_600_123_456);
        let bytes: &[u8] = &[0, 1, 2, 255];
        let row = client
            .query_one(
                "SELECT $1::bool, $2::int2, $3::int4, $4::int8, $5::float4, $6::float8, \
                 $7::text, $8::varchar, $9::bytea, $10::timestamptz, $11::uuid, $12::numeric, \
                 $13::jsonb, $14::date, $15::int4, $16::oid",
                &[
                    &true,
                    &-7i16,
                    &123_456i32,
                    &-9_000_000_000i64,
                    &1.5f32,
                    &-2.25f64,
                    &"héllo wörld",
                    &String::from("varchar"),
                    &bytes,
                    &now,
                    &"6c1b0a3e-1d2f-4a5b-8c9d-0e1f2a3b4c5d",
                    &"12345.678901234567890",
                    &r#"{"a": [1, 2]}"#,
                    &"2026-11-12",
                    &None::<i32>,
                    &42i64,
                ],
            )
            .await
            .unwrap();
        assert!(row.get::<bool>(0).unwrap());
        assert_eq!(row.get::<i16>(1).unwrap(), -7);
        assert_eq!(row.get::<i32>(2).unwrap(), 123_456);
        assert_eq!(row.get::<i64>(3).unwrap(), -9_000_000_000);
        assert_eq!(row.get::<f32>(4).unwrap(), 1.5);
        assert_eq!(row.get::<f64>(5).unwrap(), -2.25);
        assert_eq!(row.get::<&str>(6).unwrap(), "héllo wörld");
        assert_eq!(row.get::<String>(7).unwrap(), "varchar");
        assert_eq!(row.get::<&[u8]>(8).unwrap(), bytes);
        assert_eq!(row.get::<SystemTime>(9).unwrap(), now);
        assert_eq!(
            row.get::<&str>(10).unwrap(),
            "6c1b0a3e-1d2f-4a5b-8c9d-0e1f2a3b4c5d"
        );
        assert_eq!(row.get::<&str>(11).unwrap(), "12345.678901234567890");
        assert_eq!(row.get::<&str>(12).unwrap(), r#"{"a": [1, 2]}"#);
        assert_eq!(row.get::<&str>(13).unwrap(), "2026-11-12");
        assert_eq!(row.get::<Option<i32>>(14).unwrap(), None);
        assert_eq!(row.get::<i64>(15).unwrap(), 42);
    });
}

#[test]
fn types_this_crate_does_not_know_read_as_text() {
    run(|s| async move {
        let client = connect(s).await;
        let row = client
            .query_one(
                "SELECT ARRAY[1, 2, 3], interval '1 day 2 hours', inet '10.0.0.1/8', \
                 point(1, 2), 'a'::\"char\", NULL::int[]",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "{1,2,3}");
        assert_eq!(row.get::<&str>(1).unwrap(), "1 day 02:00:00");
        assert_eq!(row.get::<&str>(2).unwrap(), "10.0.0.1/8");
        assert_eq!(row.get::<&str>(3).unwrap(), "(1,2)");
        assert_eq!(row.get::<&str>(4).unwrap(), "a");
        assert_eq!(row.get::<Option<&str>>(5).unwrap(), None);
    });
}

#[test]
fn conversion_errors_name_the_parameter_or_column() {
    run(|s| async move {
        let client = connect(s).await;
        let e = client.query("SELECT $1::text", &[&1i32]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Type);
        assert!(
            e.to_string()
                .starts_with("GIONG-E3005: parameter $1 (text): "),
            "{e}"
        );

        let e = client
            .query("SELECT $1::int4", &[&3_000_000_000i64])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("does not fit"), "{e}");

        let e = client
            .query("SELECT $1::int4, $2::int4", &[&1i32])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("2 parameters but 1 values"), "{e}");

        let row = client
            .query_one("SELECT 1::int8 AS n, NULL::text AS t", &[])
            .await
            .unwrap();
        let e = row.get::<String>("n").unwrap_err();
        assert!(
            e.to_string().starts_with("GIONG-E3005: column \"n\": "),
            "{e}"
        );
        let e = row.get::<String>("t").unwrap_err();
        assert!(e.to_string().contains("Option<"), "{e}");

        // A text value the server cannot parse is the server's error.
        let e = client
            .query("SELECT $1::uuid", &[&"not-a-uuid"])
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some("22P02"), "{e}");

        let e = client.query("SELECT 1\0", &[]).await.unwrap_err();
        assert!(e.to_string().contains("NUL"), "{e}");

        // None of it broke the connection.
        assert_eq!(
            client
                .query_one("SELECT 2", &[])
                .await
                .unwrap()
                .get::<i32>(0)
                .unwrap(),
            2
        );
    });
}

#[test]
fn server_errors_carry_their_fields_and_leave_the_connection_usable() {
    run(|s| async move {
        let client = connect(s).await;
        let e = client.query("SELEKT 1", &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Db);
        assert_eq!(e.code(), Some("42601"));
        assert_eq!(e.db().unwrap().position(), Some(1));

        table(&client, "errs", "id int PRIMARY KEY").await;
        client
            .execute("INSERT INTO errs VALUES (1)", &[])
            .await
            .unwrap();
        let e = client
            .execute("INSERT INTO errs VALUES (1)", &[])
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some("23505"));
        assert_eq!(e.db().unwrap().constraint(), Some("errs_pkey"));
        assert_eq!(e.db().unwrap().table(), Some("errs"));
        assert!(e.to_string().contains("duplicate key"), "{e}");

        let n: i64 = client
            .query_one("SELECT count(*) FROM errs", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
    });
}

#[test]
fn row_counts_and_the_one_row_helpers() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "counts", "id int").await;
        let n = client
            .execute("INSERT INTO counts SELECT generate_series(1, $1)", &[&3i32])
            .await
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            client
                .execute("UPDATE counts SET id = id + 1 WHERE id > 1", &[])
                .await
                .unwrap(),
            2
        );

        let e = client
            .query_one("SELECT id FROM counts", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::RowCount);
        assert!(client
            .query_opt("SELECT id FROM counts WHERE id > 100", &[])
            .await
            .unwrap()
            .is_none());
        assert!(client
            .query_opt("SELECT id FROM counts", &[])
            .await
            .is_err());

        let rows = client
            .query("SELECT generate_series(1, 100000) AS n", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 100_000);
        assert_eq!(rows[99_999].get::<i32>("n").unwrap(), 100_000);

        // Rows without columns are rows too.
        let rows = client
            .query("SELECT FROM generate_series(1, 3)", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_empty());
        client
            .query_one("SELECT FROM counts WHERE id = 1", &[])
            .await
            .unwrap();
    });
}

#[test]
fn batch_execute_runs_several_statements_atomically() {
    run(|s| async move {
        let client = connect(s).await;
        client
            .batch_execute("DROP TABLE IF EXISTS batch; CREATE TABLE batch (id int); INSERT INTO batch VALUES (1), (2); SELECT 1;")
            .await
            .unwrap();
        let e = client
            .batch_execute("INSERT INTO batch VALUES (3); SELECT 1/0;")
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some("22012"));
        let n: i64 = client
            .query_one("SELECT count(*) FROM batch", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 2, "the insert before the error was rolled back");
        // Notices are not errors.
        client
            .batch_execute("DO $$ BEGIN RAISE NOTICE 'hello'; END $$")
            .await
            .unwrap();
    });
}

#[test]
fn tasks_sharing_a_connection_pipeline_their_statements() {
    run(|s| async move {
        let client = Rc::new(connect(s).await);
        let sql = "SELECT $1::int4 * 2";
        // Prepare it once, so that every task below sends at once.
        client.query_one(sql, &[&0i32]).await.unwrap();

        let tasks: Vec<_> = (0..200i32)
            .map(|i| {
                let client = client.clone();
                giong::spawn(async move {
                    let row = client.query_one(sql, &[&i]).await.unwrap();
                    row.get::<i32>(0).unwrap()
                })
            })
            .collect();
        giong::yield_now().await;
        assert_eq!(
            client.in_flight(),
            200,
            "all 200 requests sent before any answer"
        );
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(
                t.await.unwrap(),
                i as i32 * 2,
                "answers go to the right task"
            );
        }
        assert_eq!(client.in_flight(), 0);
    });
}

#[test]
fn a_failing_statement_in_a_pipeline_affects_only_itself() {
    run(|s| async move {
        let client = Rc::new(connect(s).await);
        let a = giong::spawn({
            let c = client.clone();
            async move {
                c.query_one("SELECT 1", &[])
                    .await
                    .map(|r| r.get::<i32>(0).unwrap())
            }
        });
        let b = giong::spawn({
            let c = client.clone();
            async move { c.query_one("SELECT 1/0", &[]).await.map(|_| 0) }
        });
        let c2 = giong::spawn({
            let c = client.clone();
            async move {
                c.query_one("SELECT 3", &[])
                    .await
                    .map(|r| r.get::<i32>(0).unwrap())
            }
        });
        assert_eq!(a.await.unwrap().unwrap(), 1);
        assert_eq!(b.await.unwrap().unwrap_err().code(), Some("22012"));
        assert_eq!(c2.await.unwrap().unwrap(), 3);
    });
}

#[test]
fn statements_are_prepared_once_and_the_cache_is_bounded() {
    run(|s| async move {
        let count = "SELECT count(*) FROM pg_prepared_statements";
        let client = Client::connect(&s.local().statement_cache_capacity(2))
            .await
            .unwrap();
        for i in 0..5 {
            client.query(&format!("SELECT {i}"), &[]).await.unwrap();
        }
        let n: i64 = client.query_one(count, &[]).await.unwrap().get(0).unwrap();
        assert!(n <= 3, "{n} statements kept with a capacity of 2");

        let client = Client::connect(&s.local().statement_cache_capacity(0))
            .await
            .unwrap();
        for i in 0..5 {
            client.query(&format!("SELECT {i}"), &[]).await.unwrap();
        }
        // A failed conversion must not leave a statement behind either.
        assert!(client.query("SELECT $1::int4", &[&"x"]).await.is_err());
        let n: i64 = client.query_one(count, &[]).await.unwrap().get(0).unwrap();
        assert_eq!(n, 1, "only the statement running now");
    });
}

#[test]
fn a_cached_statement_is_prepared_again_after_the_schema_changes() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "schema_change", "a int").await;
        client
            .execute("INSERT INTO schema_change VALUES (1)", &[])
            .await
            .unwrap();
        let sql = "SELECT * FROM schema_change";
        assert_eq!(client.query_one(sql, &[]).await.unwrap().len(), 1);
        client
            .batch_execute("ALTER TABLE schema_change ADD COLUMN b text")
            .await
            .unwrap();
        // The cached plan fails in Bind ("cached plan must not change result
        // type"), before anything ran: the statement is prepared again and
        // run once more, so the caller sees no error.
        assert_eq!(client.query_one(sql, &[]).await.unwrap().len(), 2);
        assert_eq!(client.query_one(sql, &[]).await.unwrap().len(), 2);
    });
}

#[test]
fn a_schema_change_inside_a_transaction_fails_once_and_is_not_retried() {
    run(|s| async move {
        let mut client = connect(s).await;
        table(&client, "schema_change_tx", "a int").await;
        let sql = "SELECT * FROM schema_change_tx";
        client.query(sql, &[]).await.unwrap();
        client
            .batch_execute("ALTER TABLE schema_change_tx ADD COLUMN b text")
            .await
            .unwrap();
        // The error aborted the transaction, so running the statement again
        // would only fail with 25P02: the caller gets the error, and the
        // stale statement is gone from the cache.
        let tx = client.transaction().await.unwrap();
        let e = tx.query(sql, &[]).await.unwrap_err();
        assert_eq!(e.code(), Some("0A000"), "{e}");
        assert_eq!(
            e.db().unwrap().routine(),
            Some("RevalidateCachedQuery"),
            "{e}"
        );
        tx.rollback().await.unwrap();
        // Prepared afresh, with both columns.
        client
            .execute("INSERT INTO schema_change_tx VALUES (1, 'x')", &[])
            .await
            .unwrap();
        assert_eq!(client.query_one(sql, &[]).await.unwrap().len(), 2);
    });
}

#[test]
fn a_statement_name_the_application_took_is_skipped() {
    run(|s| async move {
        let client = connect(s).await;
        // The name the client's first statement would get.
        client
            .batch_execute("PREPARE __giong_1 AS SELECT 'mine'")
            .await
            .unwrap();
        let row = client.query_one("SELECT 2 + 2", &[]).await.unwrap();
        assert_eq!(row.get::<i32>(0).unwrap(), 4);
        let row = client.query_one("EXECUTE __giong_1", &[]).await.unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "mine");
    });
}

#[test]
fn large_notices_are_skipped_and_large_errors_fail_only_their_statement() {
    run(|s| async move {
        let client = Client::connect(&s.local().max_result_size(1024))
            .await
            .unwrap();
        // Messages up to 1 MiB are read whatever max_result_size says; these
        // are 2 MB.
        client
            .batch_execute("DO $$ BEGIN RAISE NOTICE '%', repeat('x', 2000000); END $$")
            .await
            .unwrap();
        let e = client
            .batch_execute("DO $$ BEGIN RAISE EXCEPTION '%', repeat('x', 2000000); END $$")
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge, "{e}");
        assert!(!e.to_string().contains("really a PostgreSQL"), "{e}");
        assert!(!client.is_closed());
        let row = client.query_one("SELECT 1", &[]).await.unwrap();
        assert_eq!(row.get::<i32>(0).unwrap(), 1);
    });
}

#[test]
fn the_statement_timeout_can_be_left_to_the_server() {
    run(|s| async move {
        let show = |config: giong_postgres::Config| async move {
            let client = Client::connect(&config).await?;
            let row = client.query_one("SHOW statement_timeout", &[]).await?;
            row.get::<String>(0)
        };
        assert_eq!(show(s.local()).await.unwrap(), "30s");
        assert_eq!(
            show(s.local().statement_timeout(Duration::from_millis(1500)))
                .await
                .unwrap(),
            "1500ms"
        );
        // Nothing sent: the server's own setting, here its default.
        assert_eq!(show(s.local().statement_timeout(None)).await.unwrap(), "0");
        // Zero is not "no limit", and PostgreSQL takes at most i32::MAX ms:
        // both are refused before connecting.
        for bad in [Duration::ZERO, Duration::from_secs(40 * 24 * 3600)] {
            let e = show(s.local().statement_timeout(bad)).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Config, "{e}");
            assert!(e.to_string().contains("statement_timeout(None)"), "{e}");
        }
    });
}

#[test]
fn a_result_over_the_limit_fails_and_the_connection_survives() {
    run(|s| async move {
        let client = Client::connect(&s.local().max_result_size(10_000))
            .await
            .unwrap();
        let e = client
            .query("SELECT repeat('x', 100) FROM generate_series(1, 1000)", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge);
        assert!(e.to_string().contains("max_result_size"), "{e}");
        // A single row larger than any message the client will buffer.
        let e = client
            .query("SELECT repeat('x', 3000000)", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge);
        assert_eq!(
            client
                .query_one("SELECT 5", &[])
                .await
                .unwrap()
                .get::<i32>(0)
                .unwrap(),
            5
        );
    });
}

#[test]
fn the_statement_timeout_is_enforced_by_the_server() {
    run(|s| async move {
        let client = Client::connect(&s.local().statement_timeout(Duration::from_millis(100)))
            .await
            .unwrap();
        let e = client.query("SELECT pg_sleep(2)", &[]).await.unwrap_err();
        assert_eq!(e.code(), Some("57014"));
        assert!(e.to_string().contains("Config::statement_timeout"), "{e}");
        assert_eq!(
            client
                .query_one("SELECT 1", &[])
                .await
                .unwrap()
                .get::<i32>(0)
                .unwrap(),
            1
        );
    });
}

#[test]
fn a_cancel_request_stops_the_running_statement() {
    run(|s| async move {
        let client = Rc::new(connect(s).await);
        let token = client.cancel_token();
        let slow = giong::spawn({
            let client = client.clone();
            async move { client.query("SELECT pg_sleep(10)", &[]).await }
        });
        sleep(Duration::from_millis(200)).await;
        token.cancel().await.unwrap();
        let e = timeout(Duration::from_secs(5), slow)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(e.code(), Some("57014"), "{e}");
        assert!(e.to_string().contains("user request"), "{e}");
    });
}

#[test]
fn a_dropped_query_does_not_disturb_the_next_one() {
    run(|s| async move {
        let client = connect(s).await;
        for _ in 0..3 {
            let r = timeout(
                Duration::from_millis(20),
                client.query("SELECT 'abandoned', pg_sleep(0.1)", &[]),
            )
            .await;
            assert!(r.is_err(), "timed out");
            let row = client.query_one("SELECT 'next'", &[]).await.unwrap();
            assert_eq!(row.get::<&str>(0).unwrap(), "next");
        }
        // Dropped before even being prepared.
        drop(client.query("SELECT 'never awaited'", &[]));
        assert_eq!(
            client
                .query_one("SELECT 7", &[])
                .await
                .unwrap()
                .get::<i32>(0)
                .unwrap(),
            7
        );
    });
}

#[test]
fn a_connection_the_server_ends_reports_why() {
    run(|s| async move {
        let client = connect(s).await;
        let pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        let admin = connect(s).await;
        admin
            .query("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap();
        for _ in 0..100 {
            if client.is_closed() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(client.is_closed(), "the client noticed without being used");
        let e = client.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert_eq!(e.code(), Some("57P01"), "{e}");
        assert!(e.to_string().contains("administrator command"), "{e}");
    });
}

#[test]
fn unsupported_copy_fails_clearly() {
    run(|s| async move {
        let client = connect(s).await;
        let e = client
            .query("COPY (SELECT 1) TO STDOUT", &[])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("COPY"), "{e}");
        assert_eq!(
            client
                .query_one("SELECT 1", &[])
                .await
                .unwrap()
                .get::<i32>(0)
                .unwrap(),
            1
        );

        table(&client, "copy_in", "a int").await;
        let e = client
            .query("COPY copy_in FROM STDIN", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert!(e.to_string().contains("COPY FROM STDIN"), "{e}");
        assert!(client.is_closed());
    });
}

#[test]
fn changing_the_client_encoding_closes_the_connection() {
    run(|s| async move {
        let client = connect(s).await;
        let _ = client.batch_execute("SET client_encoding = 'LATIN1'").await;
        let e = client.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert!(e.to_string().contains("UTF-8"), "{e}");
    });
}

#[test]
fn transactions_commit_roll_back_and_roll_back_when_dropped() {
    run(|s| async move {
        let mut client = connect(s).await;
        table(&client, "tx", "id int").await;
        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO tx VALUES (1)", &[]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(count(&client, "tx").await, 1);

        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO tx VALUES (2)", &[]).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(count(&client, "tx").await, 1);

        {
            let tx = client.transaction().await.unwrap();
            tx.execute("INSERT INTO tx VALUES (3)", &[]).await.unwrap();
        }
        assert_eq!(count(&client, "tx").await, 1, "dropped without commit");

        let tx = client.transaction().await.unwrap();
        assert!(tx
            .execute("INSERT INTO tx VALUES (1/0)", &[])
            .await
            .is_err());
        let e = tx.commit().await.unwrap_err();
        assert!(
            e.to_string().contains("rolled back instead of committed"),
            "{e}"
        );
        // Code that branches on the SQLSTATE sees one.
        assert_eq!(e.kind(), ErrorKind::Db);
        assert_eq!(e.code(), Some("25P02"));
        assert!(e.db().is_none());

        // A transaction() future dropped while BEGIN is in flight.
        drop(client.transaction());
        let r = timeout(Duration::from_millis(1), client.transaction()).await;
        drop(r);
        let e = client.batch_execute("SAVEPOINT s").await.unwrap_err();
        assert_eq!(
            e.code(),
            Some("25P01"),
            "not in a transaction any more: {e}"
        );
    });
}

#[test]
fn dropping_the_client_closes_the_session_and_leaks_nothing() {
    run(|s| async move {
        let admin = connect(s).await;
        let config = s.local().application_name("giong-drop-test");
        let client = Client::connect(&config).await.unwrap();
        client.query_one("SELECT 1", &[]).await.unwrap();
        let sessions =
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'giong-drop-test'";
        let n: i64 = admin
            .query_one(sessions, &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
        drop(client);
        let mut n = 1;
        for _ in 0..200 {
            n = admin
                .query_one(sessions, &[])
                .await
                .unwrap()
                .get(0)
                .unwrap();
            if n == 0 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(n, 0, "the server saw the session end");
        drop(admin);
        assert_no_leaks().await;
    });
}

#[test]
fn tls_and_tcp_connections_behave_the_same() {
    run(|s| async move {
        let client = Rc::new(
            Client::connect(&s.tls("localhost", crate::server::TLS_USER))
                .await
                .unwrap(),
        );
        let tasks: Vec<_> = (0..50i64)
            .map(|i| {
                let client = client.clone();
                giong::spawn(async move {
                    let big = "y".repeat(10_000 + i as usize);
                    let row = client
                        .query_one("SELECT $1::int8, $2::text", &[&i, &big])
                        .await
                        .unwrap();
                    assert_eq!(row.get::<&str>(1).unwrap(), big);
                    row.get::<i64>(0).unwrap()
                })
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), i as i64);
        }
    });
}
