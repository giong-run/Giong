//! Statements on one connection: types, errors, pipelining, the statement
//! cache, limits, cancellation and transactions.

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, UNIX_EPOCH};

use giong::time::{sleep, timeout};
use giong_mysql::{Bits, Client, ErrorKind};

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
        .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap();
    row.get(0).unwrap()
}

/// Runs two futures at once and returns both outputs.
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ra, mut rb) = (None, None);
    poll_fn(|cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (ra.unwrap(), rb.unwrap())
}

/// A time MySQL's DATETIME cannot hold (it goes up to the year 9999).
fn after_9999() -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(300_000_000_000)
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
        table(
            &client,
            "types_rt",
            "b BOOL, ti TINYINT, si SMALLINT, mi MEDIUMINT, i INT, bi BIGINT, \
             ubi BIGINT UNSIGNED, f FLOAT, d DOUBLE, de DECIMAL(10,2), vc VARCHAR(20), \
             tx TEXT, vb VARBINARY(10), bl BLOB, dt DATETIME(6), ts TIMESTAMP(6) NULL, \
             da DATE, tm TIME, yr YEAR, js JSON, en ENUM('a','b'), bt BIT(10), n INT",
        )
        .await;
        let now = UNIX_EPOCH + Duration::from_micros(1_767_225_600_123_456);
        let bytes: &[u8] = &[0, 1, 255];
        let blob = vec![7u8; 1000];
        let changed = client
            .execute(
                "INSERT INTO types_rt VALUES \
                 (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    &true,
                    &-7i8,
                    &-300i16,
                    &70_000i32,
                    &-2_000_000_000i32,
                    &i64::MIN,
                    &u64::MAX,
                    &1.5f32,
                    &-2.25f64,
                    &"19.99",
                    &"héllo",
                    &"some text".to_owned(),
                    &bytes,
                    &blob,
                    &now,
                    &now,
                    &"2026-01-31",
                    &"-838:59:59",
                    &2026u16,
                    &r#"{"a": 1}"#,
                    &"b",
                    &0b10_1010_1010u64,
                    &None::<i32>,
                ],
            )
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = client
            .query_one("SELECT * FROM types_rt", &[])
            .await
            .unwrap();
        assert!(row.get::<bool>("b").unwrap());
        assert_eq!(row.get::<i8>("ti").unwrap(), -7);
        assert_eq!(row.get::<i64>("ti").unwrap(), -7);
        assert_eq!(row.get::<i16>("si").unwrap(), -300);
        assert_eq!(row.get::<i32>("mi").unwrap(), 70_000);
        assert_eq!(row.get::<i32>("i").unwrap(), -2_000_000_000);
        assert_eq!(row.get::<i64>("bi").unwrap(), i64::MIN);
        assert_eq!(row.get::<u64>("ubi").unwrap(), u64::MAX);
        assert_eq!(row.get::<f32>("f").unwrap(), 1.5);
        assert_eq!(row.get::<f64>("f").unwrap(), 1.5);
        assert_eq!(row.get::<f64>("d").unwrap(), -2.25);
        assert_eq!(row.get::<&str>("de").unwrap(), "19.99");
        assert_eq!(row.get::<&str>("vc").unwrap(), "héllo");
        assert_eq!(row.get::<String>("tx").unwrap(), "some text");
        assert_eq!(row.get::<&[u8]>("vb").unwrap(), bytes);
        assert_eq!(row.get::<Vec<u8>>("bl").unwrap(), blob);
        assert_eq!(row.get::<std::time::SystemTime>("dt").unwrap(), now);
        assert_eq!(row.get::<std::time::SystemTime>("ts").unwrap(), now);
        assert_eq!(
            row.get::<String>("dt").unwrap(),
            "2026-01-01 00:00:00.123456"
        );
        assert_eq!(row.get::<String>("da").unwrap(), "2026-01-31");
        assert_eq!(row.get::<String>("tm").unwrap(), "-838:59:59");
        assert_eq!(row.get::<u16>("yr").unwrap(), 2026);
        assert_eq!(row.get::<&str>("js").unwrap(), r#"{"a": 1}"#);
        assert_eq!(row.get::<&str>("en").unwrap(), "b");
        assert_eq!(row.get::<Bits>("bt").unwrap(), Bits(0b10_1010_1010));
        assert_eq!(row.get::<Option<i32>>("n").unwrap(), None);
        assert_eq!(row.get::<Option<i32>>("i").unwrap(), Some(-2_000_000_000));

        // Columns of an expression, and a parameter read back as it was
        // sent.
        let row = client
            .query_one(
                "SELECT ? + 1, CONCAT(?, '!'), 1.5 * 2, NOW(6) = UTC_TIMESTAMP(6), ?",
                &[&41i64, &"hi", &None::<&str>],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 42);
        assert_eq!(row.get::<&str>(1).unwrap(), "hi!");
        assert_eq!(row.get::<String>(2).unwrap(), "3.0");
        assert_eq!(row.get::<i64>(2).unwrap(), 3, "a whole DECIMAL");
        assert!(row.get::<bool>(3).unwrap(), "the session runs in UTC");
        assert!(row.is_null(4).unwrap());
    });
}

#[test]
fn conversions_that_lose_something_fail_and_say_what_fits() {
    run(|s| async move {
        let client = connect(s).await;
        let row = client
            .query_one(
                "SELECT CAST(-1 AS SIGNED), CAST(300 AS SIGNED), 1.25, BINARY 'x', 2, NULL",
                &[],
            )
            .await
            .unwrap();
        let e = row.get::<u64>(0).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Type, "{e}");
        assert!(
            e.to_string()
                .contains("column \"CAST(-1 AS SIGNED)\": -1 does not fit in u64; read it as i64"),
            "{e}"
        );
        assert!(row.get::<i8>(1).is_err());
        assert!(row.get::<i16>(1).is_ok());
        let e = row.get::<i64>(2).unwrap_err();
        assert!(e.to_string().contains("String"), "{e}");
        let e = row.get::<&str>(3).unwrap_err();
        assert!(e.to_string().contains("Vec<u8>"), "{e}");
        assert!(row.get::<bool>(4).is_err(), "only 0 and 1 are booleans");
        let e = row.get::<i32>(5).unwrap_err();
        assert!(e.to_string().contains("NULL"), "{e}");

        // A parameter that cannot be sent names itself.
        let e = client
            .query("SELECT ?", &[&after_9999()])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Type, "{e}");
        assert!(e.to_string().contains("parameter 1"), "{e}");
        // A wrong number of parameters is caught before sending.
        let e = client.query("SELECT ?, ?", &[&1]).await.unwrap_err();
        assert!(e.to_string().contains("2 parameters"), "{e}");
        // And the connection is still in step.
        let row = client.query_one("SELECT 5", &[]).await.unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 5);
    });
}

#[test]
fn server_errors_carry_their_fields_and_leave_the_connection_usable() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "errs", "id INT PRIMARY KEY").await;
        let e = client.query("SELEC 1", &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Db, "{e}");
        assert_eq!(e.code(), Some(1064));
        let db = e.db().unwrap();
        assert_eq!(db.sqlstate(), "42000");
        assert!(db.message().contains("SQL syntax"), "{db}");

        client
            .execute("INSERT INTO errs VALUES (1)", &[])
            .await
            .unwrap();
        let e = client
            .execute("INSERT INTO errs VALUES (?)", &[&1])
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(1062), "{e}");
        assert_eq!(e.db().unwrap().sqlstate(), "23000");
        assert_eq!(count(&client, "errs").await, 1);
    });
}

#[test]
fn row_counts_and_the_one_row_helpers() {
    run(|s| async move {
        let client = connect(s).await;
        table(
            &client,
            "counts",
            "id BIGINT AUTO_INCREMENT PRIMARY KEY, v INT",
        )
        .await;
        let id = client
            .insert("INSERT INTO counts (v) VALUES (?), (?)", &[&1, &2])
            .await
            .unwrap();
        assert_eq!(id, 1, "the id of the first row");
        let id = client
            .insert("INSERT INTO counts (v) VALUES (?)", &[&3])
            .await
            .unwrap();
        assert_eq!(id, 3);
        // Rows matched, even when the value does not change.
        let n = client
            .execute("UPDATE counts SET v = 1 WHERE id <= ?", &[&2])
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            client.execute("SELECT * FROM counts", &[]).await.unwrap(),
            3
        );
        let e = client
            .insert("UPDATE counts SET v = 0", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::RowCount, "{e}");

        let e = client
            .query_one("SELECT * FROM counts", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::RowCount, "{e}");
        let e = client
            .query_one("SELECT * FROM counts WHERE id = 99", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::RowCount, "{e}");
        assert!(client
            .query_opt("SELECT * FROM counts WHERE id = 99", &[])
            .await
            .unwrap()
            .is_none());
        assert!(client
            .query_opt("SELECT * FROM counts WHERE id = 1", &[])
            .await
            .unwrap()
            .is_some());
    });
}

#[test]
fn batch_execute_runs_several_statements_and_stops_at_the_first_error() {
    run(|s| async move {
        let client = connect(s).await;
        client
            .batch_execute(
                "DROP TABLE IF EXISTS batch; CREATE TABLE batch (id INT); \
                 INSERT INTO batch VALUES (1); SELECT * FROM batch; INSERT INTO batch VALUES (2)",
            )
            .await
            .unwrap();
        assert_eq!(count(&client, "batch").await, 2);
        let e = client
            .batch_execute(
                "INSERT INTO batch VALUES (3); INSERT INTO nope VALUES (4); \
                 INSERT INTO batch VALUES (5)",
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(1146), "{e}");
        assert_eq!(count(&client, "batch").await, 3, "the first one stays");
    });
}

#[test]
fn tasks_sharing_a_connection_pipeline_their_statements() {
    run(|s| async move {
        let client = Rc::new(connect(s).await);
        let sql = "SELECT ? * 2";
        // Prepare it once, so that every task below sends at once.
        client.query_one(sql, &[&0i64]).await.unwrap();

        let tasks: Vec<_> = (0..200i64)
            .map(|i| {
                let client = client.clone();
                giong::spawn(async move {
                    let row = client.query_one(sql, &[&i]).await.unwrap();
                    row.get::<i64>(0).unwrap()
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
                i as i64 * 2,
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
                    .map(|r| r.get::<i64>(0).unwrap())
            }
        });
        let b = giong::spawn({
            let c = client.clone();
            async move {
                c.query_one("SELECT * FROM no_such_table", &[])
                    .await
                    .map(|_| 0)
            }
        });
        let c2 = giong::spawn({
            let c = client.clone();
            async move {
                c.query_one("SELECT 3", &[])
                    .await
                    .map(|r| r.get::<i64>(0).unwrap())
            }
        });
        assert_eq!(a.await.unwrap().unwrap(), 1);
        assert_eq!(b.await.unwrap().unwrap_err().code(), Some(1146));
        assert_eq!(c2.await.unwrap().unwrap(), 3);
    });
}

/// How many prepared statements the server holds for this connection.
async fn prepared(client: &Client) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*) FROM performance_schema.prepared_statements_instances \
             WHERE OWNER_THREAD_ID = PS_CURRENT_THREAD_ID()",
            &[],
        )
        .await
        .unwrap()
        .get(0)
        .unwrap()
}

#[test]
fn statements_are_prepared_once_and_the_cache_is_bounded() {
    run(|s| async move {
        let client = Client::connect(&s.local().statement_cache_capacity(2))
            .await
            .unwrap();
        for i in 0..5 {
            client.query(&format!("SELECT {i}"), &[]).await.unwrap();
        }
        // The counting statement and the one used last.
        assert_eq!(prepared(&client).await, 2);

        let client = Client::connect(&s.local().statement_cache_capacity(0))
            .await
            .unwrap();
        for i in 0..5 {
            client.query(&format!("SELECT {i}"), &[]).await.unwrap();
        }
        // A failed conversion must not leave a statement behind either.
        assert!(client.query("SELECT ?", &[&after_9999()]).await.is_err());
        assert_eq!(prepared(&client).await, 1, "only the statement running now");
    });
}

#[test]
fn a_cached_statement_follows_schema_changes() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "schema_change", "a INT").await;
        client
            .execute("INSERT INTO schema_change VALUES (1)", &[])
            .await
            .unwrap();
        let sql = "SELECT * FROM schema_change";
        assert_eq!(client.query_one(sql, &[]).await.unwrap().len(), 1);
        client
            .batch_execute("ALTER TABLE schema_change ADD COLUMN b TEXT")
            .await
            .unwrap();
        // MySQL prepares it again by itself; the rows describe the new
        // columns.
        let row = client.query_one(sql, &[]).await.unwrap();
        assert_eq!(row.len(), 2);
        assert_eq!(row.columns()[1].name(), "b");
    });
}

#[test]
fn large_values_cross_the_16_mib_packet_limit_both_ways() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "large", "id INT, v LONGBLOB").await;
        for len in [0xFF_FFFF - 30, 0xFF_FFFF, 20 << 20] {
            let value: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            client
                .execute("INSERT INTO large VALUES (?, ?)", &[&(len as i64), &value])
                .await
                .unwrap();
            let row = client
                .query_one("SELECT v FROM large WHERE id = ?", &[&(len as i64)])
                .await
                .unwrap();
            assert!(
                row.get::<&[u8]>(0).unwrap() == value.as_slice(),
                "{len} bytes"
            );
        }
    });
}

#[test]
fn a_result_over_the_limit_fails_and_the_connection_survives() {
    run(|s| async move {
        let client = Client::connect(&s.local().max_result_size(10_000))
            .await
            .unwrap();
        let rows = "WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n \
                    WHERE i < 1000) SELECT REPEAT('x', 100) FROM n";
        let e = client.query(rows, &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge);
        assert!(e.to_string().contains("max_result_size"), "{e}");
        // A single row larger than any message the client will buffer.
        let e = client
            .query("SELECT REPEAT('x', 3000000)", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge);
        // Not a limit on statements that do not keep their rows.
        assert_eq!(client.execute(rows, &[]).await.unwrap(), 1000);
        assert_eq!(
            client
                .query_one("SELECT 5", &[])
                .await
                .unwrap()
                .get::<i64>(0)
                .unwrap(),
            5
        );
    });
}

/// A SELECT that takes a few seconds of CPU.
const SLOW_SELECT: &str = "WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n \
                           WHERE i < 100000000) SELECT COUNT(*) FROM n";

#[test]
fn the_statement_timeout_is_enforced_by_the_server() {
    run(|s| async move {
        let config = s
            .local()
            .statement_timeout(Duration::from_millis(100))
            .session_variable("cte_max_recursion_depth", "4294967295");
        let client = Client::connect(&config).await.unwrap();
        let started = std::time::Instant::now();
        let e = client.query(SLOW_SELECT, &[]).await.unwrap_err();
        assert_eq!(e.code(), Some(3024), "{e}");
        assert!(e.to_string().contains("Config::statement_timeout"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            client
                .query_one("SELECT 1", &[])
                .await
                .unwrap()
                .get::<i64>(0)
                .unwrap(),
            1
        );
    });
}

#[test]
fn a_cancel_request_stops_the_running_statement() {
    run(|s| async move {
        let client = Rc::new(
            Client::connect(
                &s.local()
                    .session_variable("cte_max_recursion_depth", "4294967295"),
            )
            .await
            .unwrap(),
        );
        let token = client.cancel_token();
        let slow = giong::spawn({
            let client = client.clone();
            async move { client.query(SLOW_SELECT, &[]).await }
        });
        sleep(Duration::from_millis(200)).await;
        token.cancel().await.unwrap();
        let e = timeout(Duration::from_secs(5), slow)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(e.code(), Some(1317), "{e}");
        // The connection stays usable.
        let row = client.query_one("SELECT 2", &[]).await.unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 2);
    });
}

#[test]
fn a_dropped_query_does_not_disturb_the_next_one() {
    run(|s| async move {
        let client = connect(s).await;
        for _ in 0..3 {
            let r = timeout(
                Duration::from_millis(20),
                client.query("SELECT 'abandoned', SLEEP(0.1)", &[]),
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
                .get::<i64>(0)
                .unwrap(),
            7
        );
    });
}

#[test]
fn a_connection_the_server_ends_reports_why() {
    run(|s| async move {
        let client = connect(s).await;
        let admin = connect(s).await;
        admin
            .batch_execute(&format!("KILL {}", client.connection_id()))
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
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
    });
}

#[test]
fn stored_procedures_give_their_first_result() {
    run(|s| async move {
        let client = connect(s).await;
        client
            .batch_execute(
                "DROP PROCEDURE IF EXISTS two_results; \
                 CREATE PROCEDURE two_results(IN x INT) \
                 BEGIN SELECT x AS first; SELECT x * 2 AS second, 'b'; END",
            )
            .await
            .unwrap();
        let rows = client.query("CALL two_results(?)", &[&21]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].columns()[0].name(), "first");
        assert_eq!(rows[0].get::<i64>(0).unwrap(), 21);
        // The rest was read and the connection is in step.
        let row = client.query_one("SELECT 'after'", &[]).await.unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "after");
    });
}

#[test]
fn local_files_are_never_sent() {
    run(|s| async move {
        let client = connect(s).await;
        table(&client, "local_rows", "v TEXT").await;
        let e = client
            .batch_execute("LOAD DATA LOCAL INFILE '/etc/hostname' INTO TABLE local_rows")
            .await
            .unwrap_err();
        // The server refuses (the client did not offer local files) or asks
        // anyway, in which case the client closes the connection.
        assert!(matches!(e.kind(), ErrorKind::Db | ErrorKind::Closed), "{e}");
    });
}

#[test]
fn session_variables_are_set_when_connecting() {
    run(|s| async move {
        let config = s
            .local()
            .session_variable("sql_mode", "'STRICT_ALL_TABLES'")
            .session_variable("group_concat_max_len", "4096");
        let client = Client::connect(&config).await.unwrap();
        let row = client
            .query_one(
                "SELECT @@sql_mode, @@group_concat_max_len, @@max_execution_time",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "STRICT_ALL_TABLES");
        assert_eq!(row.get::<u64>(1).unwrap(), 4096);
        assert_eq!(row.get::<u64>(2).unwrap(), 30_000, "the statement timeout");
    });
}

#[test]
fn transactions_commit_roll_back_and_roll_back_when_dropped() {
    run(|s| async move {
        let mut client = connect(s).await;
        table(&client, "tx", "id INT").await;
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

        // A failed statement does not end a MySQL transaction.
        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO tx VALUES (4)", &[]).await.unwrap();
        assert!(tx
            .execute("INSERT INTO nope VALUES (1)", &[])
            .await
            .is_err());
        tx.commit().await.unwrap();
        assert_eq!(count(&client, "tx").await, 2);

        // A transaction() future dropped while BEGIN is in flight.
        drop(client.transaction());
        let r = timeout(Duration::from_millis(1), client.transaction()).await;
        drop(r);
        // Not in a transaction any more: another session sees the row.
        client
            .execute("INSERT INTO tx VALUES (5)", &[])
            .await
            .unwrap();
        assert_eq!(s.sql("SELECT COUNT(*) FROM giong.tx").trim(), "3");
    });
}

#[test]
fn a_transaction_the_server_committed_cannot_go_on_silently() {
    run(|s| async move {
        let mut client = connect(s).await;
        table(&client, "ddl_tx", "id INT").await;
        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO ddl_tx VALUES (1)", &[])
            .await
            .unwrap();
        // DDL commits the transaction.
        tx.batch_execute("CREATE TABLE IF NOT EXISTS ddl_tx_other (id INT)")
            .await
            .unwrap();
        let e = tx
            .execute("INSERT INTO ddl_tx VALUES (2)", &[])
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("Keep DDL out of transactions"),
            "{e}"
        );
        let e = tx.commit().await.unwrap_err();
        assert!(e.to_string().contains("commit it"), "{e}");
        // Only the row before the DDL is there, and the client works again.
        assert_eq!(count(&client, "ddl_tx").await, 1);
    });
}

#[test]
fn a_deadlock_ends_the_transaction_and_says_to_retry() {
    run(|s| async move {
        let mut a = connect(s).await;
        let mut b = connect(s).await;
        table(&a, "deadlock", "id INT PRIMARY KEY, v INT").await;
        a.execute("INSERT INTO deadlock VALUES (1, 0), (2, 0)", &[])
            .await
            .unwrap();
        let ta = a.transaction().await.unwrap();
        let tb = b.transaction().await.unwrap();
        ta.execute("UPDATE deadlock SET v = 1 WHERE id = 1", &[])
            .await
            .unwrap();
        tb.execute("UPDATE deadlock SET v = 2 WHERE id = 2", &[])
            .await
            .unwrap();
        // Each now waits for the other's row; InnoDB rolls one back.
        let (ra, rb) = both(
            ta.execute("UPDATE deadlock SET v = 1 WHERE id = 2", &[]),
            tb.execute("UPDATE deadlock SET v = 2 WHERE id = 1", &[]),
        )
        .await;
        let (loser, winner, e) = match (ra, rb) {
            (Err(e), Ok(_)) => (ta, tb, e),
            (Ok(_), Err(e)) => (tb, ta, e),
            other => panic!("expected one deadlock: {other:?}"),
        };
        assert_eq!(e.code(), Some(1213), "{e}");
        // The loser's transaction is gone: nothing more runs in it.
        let e = loser
            .execute("UPDATE deadlock SET v = 9 WHERE id = 1", &[])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("rolled back by the server"), "{e}");
        assert!(loser.commit().await.is_err());
        winner.commit().await.unwrap();
        let rows = a
            .query("SELECT v FROM deadlock ORDER BY id", &[])
            .await
            .unwrap();
        let v: Vec<i64> = rows.iter().map(|r| r.get(0).unwrap()).collect();
        assert!(
            v == [1, 1] || v == [2, 2],
            "only the winner's changes: {v:?}"
        );
    });
}

#[test]
fn dropping_the_client_closes_the_session_and_leaks_nothing() {
    run(|s| async move {
        let admin = connect(s).await;
        let client = connect(s).await;
        let id = client.connection_id();
        client.query_one("SELECT 1", &[]).await.unwrap();
        let sessions = "SELECT COUNT(*) FROM performance_schema.threads WHERE PROCESSLIST_ID = ?";
        let n: i64 = admin
            .query_one(sessions, &[&id])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
        drop(client);
        let mut n = 1;
        for _ in 0..200 {
            n = admin
                .query_one(sessions, &[&id])
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
            Client::connect(&s.tls("localhost", crate::server::SHA2_USER))
                .await
                .unwrap(),
        );
        let tasks: Vec<_> = (0..50i64)
            .map(|i| {
                let client = client.clone();
                giong::spawn(async move {
                    let big = "y".repeat(10_000 + i as usize);
                    let row = client.query_one("SELECT ?, ?", &[&i, &big]).await.unwrap();
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
