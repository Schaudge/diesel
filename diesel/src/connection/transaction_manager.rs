use crate::backend::UsesAnsiSavepointSyntax;
use crate::connection::{Connection, SimpleConnection};
use crate::result::{DatabaseErrorKind, Error, QueryResult};

/// Manages the internal transaction state for a connection.
///
/// You will not need to interact with this trait, unless you are writing an
/// implementation of [`Connection`](trait.Connection.html).
pub trait TransactionManager<Conn: Connection> {
    /// Begin a new transaction or savepoint
    ///
    /// If the transaction depth is greater than 0,
    /// this should create a savepoint instead.
    /// This function is expected to increment the transaction depth by 1.
    fn begin_transaction(&self, conn: &Conn) -> QueryResult<()>;

    /// Rollback the inner-most transaction or savepoint
    ///
    /// If the transaction depth is greater than 1,
    /// this should rollback to the most recent savepoint.
    /// This function is expected to decrement the transaction depth by 1.
    fn rollback_transaction(&self, conn: &Conn) -> QueryResult<()>;

    /// Commit the inner-most transaction or savepoint
    ///
    /// If the transaction depth is greater than 1,
    /// this should release the most recent savepoint.
    /// This function is expected to decrement the transaction depth by 1.
    fn commit_transaction(&self, conn: &Conn) -> QueryResult<()>;

    /// Fetch the current transaction depth
    ///
    /// Used to ensure that `begin_test_transaction` is not called when already
    /// inside of a transaction.
    fn get_transaction_depth(&self) -> u32;
}

use std::cell::Cell;

/// An implementation of `TransactionManager` which can be used for backends
/// which use ANSI standard syntax for savepoints such as SQLite and PostgreSQL.
#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct AnsiTransactionManager {
    transaction_depth: Cell<i32>,
}

impl AnsiTransactionManager {
    /// Create a new transaction manager
    pub fn new() -> Self {
        AnsiTransactionManager::default()
    }

    fn change_transaction_depth(&self, by: i32, query: QueryResult<()>) -> QueryResult<()> {
        if query.is_ok() {
            self.transaction_depth
                .set(self.transaction_depth.get() + by)
        }
        query
    }

    /// Begin a transaction with custom SQL
    ///
    /// This is used by connections to implement more complex transaction APIs
    /// to set things such as isolation levels.
    /// Returns an error if already inside of a transaction.
    pub fn begin_transaction_sql<Conn>(&self, conn: &Conn, sql: &str) -> QueryResult<()>
    where
        Conn: SimpleConnection,
    {
        use crate::result::Error::AlreadyInTransaction;

        if self.transaction_depth.get() == 0 {
            self.change_transaction_depth(1, conn.batch_execute(sql))
        } else {
            Err(AlreadyInTransaction)
        }
    }
}

impl<Conn> TransactionManager<Conn> for AnsiTransactionManager
where
    Conn: Connection,
    Conn::Backend: UsesAnsiSavepointSyntax,
{
    fn begin_transaction(&self, conn: &Conn) -> QueryResult<()> {
        let transaction_depth = self.transaction_depth.get();
        self.change_transaction_depth(
            1,
            if transaction_depth == 0 {
                conn.batch_execute("BEGIN")
            } else {
                conn.batch_execute(&format!("SAVEPOINT diesel_savepoint_{}", transaction_depth))
            },
        )
    }

    fn rollback_transaction(&self, conn: &Conn) -> QueryResult<()> {
        let transaction_depth = self.transaction_depth.get();
        self.change_transaction_depth(
            -1,
            if transaction_depth == 1 {
                conn.batch_execute("ROLLBACK")
            } else {
                conn.batch_execute(&format!(
                    "ROLLBACK TO SAVEPOINT diesel_savepoint_{}",
                    transaction_depth - 1
                ))
            },
        )
    }

    /// If the transaction fails to commit due to a `SerializationFailure` or a
    /// `ReadOnlyTransaction` a rollback will be attempted. If the rollback succeeds,
    /// the original error will be returned, otherwise the error generated by the rollback
    /// will be returned. In the second case the connection should be considered broken
    /// as it contains a uncommitted unabortable open transaction.
    fn commit_transaction(&self, conn: &Conn) -> QueryResult<()> {
        let transaction_depth = self.transaction_depth.get();
        if transaction_depth <= 1 {
            match conn.batch_execute("COMMIT") {
                // When any of these kinds of error happen on `COMMIT`, it is expected
                // that a `ROLLBACK` would succeed, leaving the transaction in a non-broken state.
                // If there are other such errors, it is fine to add them here.
                e @ Err(Error::DatabaseError(DatabaseErrorKind::SerializationFailure, _))
                | e @ Err(Error::DatabaseError(DatabaseErrorKind::ReadOnlyTransaction, _)) => {
                    self.change_transaction_depth(-1, conn.batch_execute("ROLLBACK"))?;
                    e
                }
                result => self.change_transaction_depth(-1, result),
            }
        } else {
            self.change_transaction_depth(
                -1,
                conn.batch_execute(&format!(
                    "RELEASE SAVEPOINT diesel_savepoint_{}",
                    transaction_depth - 1
                )),
            )
        }
    }

    fn get_transaction_depth(&self) -> u32 {
        self.transaction_depth.get() as u32
    }
}

#[cfg(test)]
mod test {
    #[cfg(feature = "postgres")]
    macro_rules! matches {
        ($expression:expr, $( $pattern:pat )|+ $( if $guard: expr )?) => {
            match $expression {
                $( $pattern )|+ $( if $guard )? => true,
                _ => false
            }
        }
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn transaction_depth_is_tracked_properly_on_commit_failure() {
        use crate::result::DatabaseErrorKind::SerializationFailure;
        use crate::result::Error::DatabaseError;
        use crate::*;
        use std::sync::{Arc, Barrier};
        use std::thread;

        table! {
            #[sql_name = "transaction_depth_is_tracked_properly_on_commit_failure"]
            serialization_example {
                id -> Serial,
                class -> Integer,
            }
        }

        let conn = crate::test_helpers::pg_connection_no_transaction();

        sql_query("DROP TABLE IF EXISTS transaction_depth_is_tracked_properly_on_commit_failure;")
            .execute(&conn)
            .unwrap();
        sql_query(
            r#"
            CREATE TABLE transaction_depth_is_tracked_properly_on_commit_failure (
                id SERIAL PRIMARY KEY,
                class INTEGER NOT NULL
            )
        "#,
        )
        .execute(&conn)
        .unwrap();

        insert_into(serialization_example::table)
            .values(&vec![
                serialization_example::class.eq(1),
                serialization_example::class.eq(2),
            ])
            .execute(&conn)
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let threads = (1..3)
            .map(|i| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    use crate::connection::transaction_manager::AnsiTransactionManager;
                    use crate::connection::transaction_manager::TransactionManager;
                    let conn = crate::test_helpers::pg_connection_no_transaction();
                    assert_eq!(0, <AnsiTransactionManager as TransactionManager<PgConnection>>::get_transaction_depth(&conn.transaction_manager));

                    let result =
                    conn.build_transaction().serializable().run(|| {
                        assert_eq!(1, <AnsiTransactionManager as TransactionManager<PgConnection>>::get_transaction_depth(&conn.transaction_manager));

                        let _ = serialization_example::table
                            .filter(serialization_example::class.eq(i))
                            .count()
                            .execute(&conn)?;

                        barrier.wait();

                        let other_i = if i == 1 { 2 } else { 1 };
                        insert_into(serialization_example::table)
                            .values(serialization_example::class.eq(other_i))
                            .execute(&conn)
                    });

                    assert_eq!(0, <AnsiTransactionManager as TransactionManager<PgConnection>>::get_transaction_depth(&conn.transaction_manager));
                    result
                })
            })
            .collect::<Vec<_>>();

        let mut results = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>();

        results.sort_by_key(|r| r.is_err());

        assert!(matches!(results[0], Ok(_)));
        assert!(matches!(
            results[1],
            Err(DatabaseError(SerializationFailure, _))
        ));
    }
}
