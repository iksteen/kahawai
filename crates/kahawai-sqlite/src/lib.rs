//! One serialized SQLite writer alongside a read-only WAL reader pool.
//!
//! SQLite permits many readers but only one writer. Letting every pooled
//! connection attempt that writer slot makes ordinary application
//! concurrency surface as `SQLITE_BUSY`, and a deferred transaction can fail
//! while upgrading from read to write without honoring the busy timeout. This
//! handle instead owns exactly one writable connection in a bounded FIFO actor
//! and starts multi-statement writes with `BEGIN IMMEDIATE`.
//!
//! The connection budget is explicit: callers choose the reader count and the
//! actor adds one writer. Reader connections must also be opened with
//! `PRAGMA query_only=ON`; disk databases should additionally use OS-level
//! read-only connections. A wrongly routed write therefore fails immediately
//! instead of silently bypassing serialization.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use either::Either;
use futures_util::TryStreamExt as _;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use sqlx::sqlite::{SqliteArguments, SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Connection as _, Execute, Executor, Sqlite, SqliteConnection, SqlitePool};
use tokio::sync::{mpsc, oneshot};

pub const WRITER_QUEUE_CAPACITY: usize = 256;
const SLOW_OPERATION: Duration = Duration::from_secs(1);

tokio::task_local! {
    static WRITER_CONTEXT: ();
}

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("SQLite writer task stopped")]
    Stopped,
    #[error("nested SQLite writer request would deadlock")]
    Nested,
}

#[derive(Clone, Debug)]
struct Writer {
    requests: mpsc::Sender<Request>,
    active_task: Arc<Mutex<Option<tokio::task::Id>>>,
}

#[derive(Debug)]
enum Request {
    Lease {
        label: String,
        queued_at: Instant,
        task_id: Option<tokio::task::Id>,
        response: oneshot::Sender<LeaseParts>,
    },
    Close(oneshot::Sender<()>),
}

#[derive(Debug)]
struct LeaseParts {
    connection: SqliteConnection,
    returned: oneshot::Sender<ReturnedConnection>,
    label: String,
    started_at: Instant,
}

#[derive(Debug)]
struct ReturnedConnection {
    connection: SqliteConnection,
    rollback: bool,
}

#[derive(Debug)]
pub struct WriterLease {
    connection: Option<SqliteConnection>,
    returned: Option<oneshot::Sender<ReturnedConnection>>,
    active_task: Arc<Mutex<Option<tokio::task::Id>>>,
    label: String,
    started_at: Instant,
    rollback: bool,
}

impl Deref for WriterLease {
    type Target = SqliteConnection;

    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("writer lease connection")
    }
}

impl DerefMut for WriterLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("writer lease connection")
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        if elapsed > SLOW_OPERATION {
            tracing::warn!(
                operation = %self.label,
                elapsed_ms = elapsed.as_millis(),
                "slow serialized SQLite writer operation"
            );
        }
        let Some(connection) = self.connection.take() else {
            return;
        };
        // Clear ownership before waking a caller that may immediately submit
        // its next independent write. Leaving this to the actor creates a
        // small false-nesting window between lease drop and return handling.
        *self.active_task.lock().unwrap() = None;
        if let Some(returned) = self.returned.take() {
            let _ = returned.send(ReturnedConnection {
                connection,
                rollback: self.rollback,
            });
        }
    }
}

impl Writer {
    fn spawn(connection: SqliteConnection) -> Self {
        let (requests, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let active_task = Arc::new(Mutex::new(None));
        tokio::spawn(writer_task(connection, receiver, active_task.clone()));
        Self {
            requests,
            active_task,
        }
    }

    async fn lease(
        &self,
        label: impl Into<String>,
    ) -> std::result::Result<WriterLease, WriterError> {
        if WRITER_CONTEXT.try_with(|()| ()).is_ok() {
            return Err(WriterError::Nested);
        }
        let task_id = tokio::task::try_id();
        if task_id.is_some() && *self.active_task.lock().unwrap() == task_id {
            return Err(WriterError::Nested);
        }
        let (response, receive) = oneshot::channel();
        self.requests
            .send(Request::Lease {
                label: label.into(),
                queued_at: Instant::now(),
                task_id,
                response,
            })
            .await
            .map_err(|_| WriterError::Stopped)?;
        let parts = receive.await.map_err(|_| WriterError::Stopped)?;
        Ok(WriterLease {
            connection: Some(parts.connection),
            returned: Some(parts.returned),
            active_task: self.active_task.clone(),
            label: parts.label,
            started_at: parts.started_at,
            rollback: false,
        })
    }

    async fn close(&self) {
        let (done, wait) = oneshot::channel();
        if self.requests.send(Request::Close(done)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

async fn writer_task(
    mut connection: SqliteConnection,
    mut requests: mpsc::Receiver<Request>,
    active_task: Arc<Mutex<Option<tokio::task::Id>>>,
) {
    while let Some(request) = requests.recv().await {
        match request {
            Request::Close(done) => {
                if let Err(error) = connection.close().await {
                    tracing::warn!(%error, "closing serialized SQLite writer connection");
                }
                let _ = done.send(());
                return;
            }
            Request::Lease {
                label,
                queued_at,
                task_id,
                response,
            } => {
                if response.is_closed() {
                    continue;
                }
                let waited = queued_at.elapsed();
                if waited > SLOW_OPERATION {
                    tracing::warn!(
                        operation = %label,
                        waited_ms = waited.as_millis(),
                        "serialized SQLite writer queue wait"
                    );
                }
                let (returned, receive) = oneshot::channel();
                *active_task.lock().unwrap() = task_id;
                let parts = LeaseParts {
                    connection,
                    returned,
                    label,
                    started_at: Instant::now(),
                };
                if let Err(parts) = response.send(parts) {
                    connection = parts.connection;
                    *active_task.lock().unwrap() = None;
                    continue;
                }
                let Ok(returned) = receive.await else {
                    tracing::error!(
                        "SQLite writer lease vanished without returning its connection"
                    );
                    break;
                };
                connection = returned.connection;
                if returned.rollback
                    && let Err(error) = sqlx::query("ROLLBACK").execute(&mut connection).await
                {
                    tracing::error!(%error, "rolling back abandoned SQLite writer transaction");
                }
                *active_task.lock().unwrap() = None;
            }
        }
    }
}

/// A database with a query-only reader pool and one serialized writer.
#[derive(Clone, Debug)]
pub struct Database {
    readers: SqlitePool,
    writer: Writer,
}

impl Database {
    /// Open one writable connection and `reader_connections` query-only ones.
    pub async fn connect_with(
        writer_options: SqliteConnectOptions,
        reader_options: SqliteConnectOptions,
        reader_connections: u32,
    ) -> Result<Self> {
        let writer = SqliteConnection::connect_with(&writer_options).await?;
        let readers = SqlitePoolOptions::new()
            .max_connections(reader_connections)
            .connect_with(reader_options)
            .await?;
        Ok(Self {
            readers,
            writer: Writer::spawn(writer),
        })
    }

    pub fn read_pool(&self) -> &SqlitePool {
        &self.readers
    }

    /// Borrow the sole writable connection for helpers that already accept a
    /// `SqliteConnection`. The lease remains actor-serialized and returns the
    /// connection on drop; new multi-statement code should prefer
    /// [`Database::transaction`] for atomicity.
    pub async fn acquire(&self) -> Result<WriterLease, sqlx::Error> {
        self.writer
            .lease("writer connection")
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))
    }

    /// Run a special or non-transactional operation on the sole writer.
    pub async fn write<R, F>(&self, label: impl Into<String>, operation: F) -> Result<R>
    where
        R: Send,
        F: for<'connection> FnOnce(
                &'connection mut SqliteConnection,
            ) -> BoxFuture<'connection, Result<R>>
            + Send,
    {
        let mut lease = self.writer.lease(label).await?;
        WRITER_CONTEXT.scope((), operation(&mut lease)).await
    }

    /// Run an atomic write after claiming SQLite's writer slot up front.
    pub async fn transaction<R, F>(&self, label: impl Into<String>, operation: F) -> Result<R>
    where
        R: Send,
        F: for<'connection> FnOnce(
                &'connection mut SqliteConnection,
            ) -> BoxFuture<'connection, Result<R>>
            + Send,
    {
        let mut transaction = self.begin_with_label(label).await?;
        let result = WRITER_CONTEXT.scope((), operation(&mut transaction)).await;
        match result {
            Ok(value) => {
                transaction.commit().await?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn begin(&self) -> Result<WriterTransaction, sqlx::Error> {
        self.begin_with_label("transaction").await
    }

    /// Compatibility with SQLx's pool API while keeping the writer actor as
    /// the owner of the transaction. Runtime callers use this when the first
    /// statement is a read and therefore require `BEGIN IMMEDIATE`.
    pub async fn begin_with(&self, statement: &str) -> Result<WriterTransaction, sqlx::Error> {
        if !statement.trim().eq_ignore_ascii_case("BEGIN IMMEDIATE") {
            return Err(sqlx::Error::Protocol(format!(
                "serialized SQLite transactions require BEGIN IMMEDIATE, not {statement}"
            )));
        }
        self.begin_with_label("immediate transaction").await
    }

    pub async fn begin_with_label(
        &self,
        label: impl Into<String>,
    ) -> Result<WriterTransaction, sqlx::Error> {
        let mut lease = self
            .writer
            .lease(label)
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *lease).await?;
        lease.rollback = true;
        Ok(WriterTransaction { lease })
    }

    pub async fn close(self) {
        self.readers.close().await;
        self.writer.close().await;
    }
}

impl Deref for Database {
    type Target = SqlitePool;

    fn deref(&self) -> &Self::Target {
        &self.readers
    }
}

/// A transaction holding the actor's only writer lease.
#[derive(Debug)]
pub struct WriterTransaction {
    lease: WriterLease,
}

impl WriterTransaction {
    pub async fn commit(mut self) -> Result<(), sqlx::Error> {
        sqlx::query("COMMIT").execute(&mut *self.lease).await?;
        self.lease.rollback = false;
        Ok(())
    }

    pub async fn rollback(mut self) -> Result<(), sqlx::Error> {
        sqlx::query("ROLLBACK").execute(&mut *self.lease).await?;
        self.lease.rollback = false;
        Ok(())
    }
}

impl Deref for WriterTransaction {
    type Target = SqliteConnection;

    fn deref(&self) -> &Self::Target {
        &self.lease
    }
}

impl DerefMut for WriterTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lease
    }
}

fn is_read_only(sql: &str) -> bool {
    let sql = sql.trim_start();
    let keyword = sql
        .split(|character: char| character.is_ascii_whitespace() || character == '(')
        .next()
        .unwrap_or_default();
    keyword.eq_ignore_ascii_case("SELECT")
        || keyword.eq_ignore_ascii_case("EXPLAIN")
        || keyword.eq_ignore_ascii_case("VALUES")
}

fn writer_label(sql: &str) -> String {
    let mut words = sql.split_whitespace();
    let label = words.by_ref().take(8).collect::<Vec<_>>().join(" ");
    if words.next().is_some() {
        format!("{label} …")
    } else {
        label
    }
}

fn take_query<'q, E>(mut query: E) -> (sqlx::SqlStr, Result<Option<SqliteArguments>, sqlx::Error>)
where
    E: Execute<'q, Sqlite>,
{
    let arguments = query.take_arguments().map_err(sqlx::Error::Encode);
    (query.sql(), arguments)
}

impl<'database> Executor<'database> for &'database Database {
    type Database = Sqlite;

    fn fetch_many<'execute, 'query: 'execute, E>(
        self,
        query: E,
    ) -> BoxStream<
        'execute,
        Result<Either<sqlx::sqlite::SqliteQueryResult, sqlx::sqlite::SqliteRow>, sqlx::Error>,
    >
    where
        'database: 'execute,
        E: 'query + Execute<'query, Sqlite>,
    {
        let (sql, arguments) = take_query(query);
        let read_only = is_read_only(sql.as_str());
        let database = self.clone();
        Box::pin(async_stream::try_stream! {
            let arguments = arguments?;
            if read_only {
                let mut connection = database.readers.acquire().await?;
                let mut rows = (&mut *connection).fetch_many((sql, arguments));
                while let Some(row) = rows.try_next().await? {
                    yield row;
                }
            } else {
                let mut connection = database.writer.lease(writer_label(sql.as_str())).await
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                let mut rows = (&mut *connection).fetch_many((sql, arguments));
                while let Some(row) = rows.try_next().await? {
                    yield row;
                }
            }
        })
    }

    fn fetch_optional<'execute, 'query: 'execute, E>(
        self,
        query: E,
    ) -> BoxFuture<'execute, Result<Option<sqlx::sqlite::SqliteRow>, sqlx::Error>>
    where
        'database: 'execute,
        E: 'query + Execute<'query, Sqlite>,
    {
        let (sql, arguments) = take_query(query);
        let read_only = is_read_only(sql.as_str());
        let database = self.clone();
        Box::pin(async move {
            let arguments = arguments?;
            if read_only {
                database
                    .readers
                    .acquire()
                    .await?
                    .fetch_optional((sql, arguments))
                    .await
            } else {
                let mut connection = database
                    .writer
                    .lease(writer_label(sql.as_str()))
                    .await
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                connection.fetch_optional((sql, arguments)).await
            }
        })
    }

    fn prepare_with<'execute>(
        self,
        sql: sqlx::SqlStr,
        parameters: &'execute [sqlx::sqlite::SqliteTypeInfo],
    ) -> BoxFuture<'execute, Result<sqlx::sqlite::SqliteStatement, sqlx::Error>>
    where
        'database: 'execute,
    {
        self.readers.prepare_with(sql, parameters)
    }

    fn describe<'execute>(
        self,
        sql: sqlx::SqlStr,
    ) -> BoxFuture<'execute, Result<sqlx::Describe<Sqlite>, sqlx::Error>>
    where
        'database: 'execute,
    {
        self.readers.describe(sql)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
    use tokio::sync::Notify;

    use super::*;

    async fn database() -> (tempfile::TempDir, Database) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.db");
        let writer = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let reader = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .pragma("query_only", "on");
        let database = Database::connect_with(writer, reader, 3).await.unwrap();
        sqlx::query("CREATE TABLE events(sequence INTEGER PRIMARY KEY, value TEXT UNIQUE)")
            .execute(&database)
            .await
            .unwrap();
        (directory, database)
    }

    #[tokio::test]
    async fn exposed_pool_rejects_writes() {
        let (_directory, database) = database().await;
        let error = sqlx::query("INSERT INTO events VALUES(1, 'bypass')")
            .execute(database.read_pool())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("readonly"));
    }

    #[tokio::test]
    async fn readers_continue_during_a_wal_write() {
        let (_directory, database) = database().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let writer = {
            let database = database.clone();
            let entered = entered.clone();
            let release = release.clone();
            tokio::spawn(async move {
                database
                    .transaction("held writer", move |connection| {
                        Box::pin(async move {
                            sqlx::query("INSERT INTO events VALUES(1, 'pending')")
                                .execute(&mut *connection)
                                .await?;
                            entered.notify_one();
                            release.notified().await;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap();
            })
        };
        entered.notified().await;
        let count: i64 = tokio::time::timeout(
            Duration::from_millis(250),
            sqlx::query_scalar("SELECT count(*) FROM events").fetch_one(&database),
        )
        .await
        .expect("reader waited for writer")
        .unwrap();
        assert_eq!(count, 0, "reader observed uncommitted data");
        release.notify_one();
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn queued_writers_are_fifo_and_never_busy() {
        let (_directory, database) = database().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let database = database.clone();
            let entered = entered.clone();
            let release = release.clone();
            tokio::spawn(async move {
                database
                    .transaction("first", move |connection| {
                        Box::pin(async move {
                            sqlx::query("INSERT INTO events VALUES(1, 'first')")
                                .execute(&mut *connection)
                                .await?;
                            entered.notify_one();
                            release.notified().await;
                            Ok(())
                        })
                    })
                    .await
            })
        };
        entered.notified().await;
        let second = {
            let database = database.clone();
            tokio::spawn(async move {
                sqlx::query("INSERT INTO events VALUES(2, 'second')")
                    .execute(&database)
                    .await
            })
        };
        tokio::task::yield_now().await;
        let third = {
            let database = database.clone();
            tokio::spawn(async move {
                sqlx::query("INSERT INTO events VALUES(3, 'third')")
                    .execute(&database)
                    .await
            })
        };
        release.notify_one();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        third.await.unwrap().unwrap();
        let values: Vec<String> = sqlx::query_scalar("SELECT value FROM events ORDER BY sequence")
            .fetch_all(&database)
            .await
            .unwrap();
        assert_eq!(values, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn cancelled_queued_work_is_skipped() {
        let (_directory, database) = database().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let database = database.clone();
            let entered = entered.clone();
            let release = release.clone();
            tokio::spawn(async move {
                database
                    .transaction("holder", move |connection| {
                        Box::pin(async move {
                            entered.notify_one();
                            release.notified().await;
                            sqlx::query("INSERT INTO events VALUES(1, 'holder')")
                                .execute(&mut *connection)
                                .await?;
                            Ok(())
                        })
                    })
                    .await
            })
        };
        entered.notified().await;
        let cancelled = {
            let database = database.clone();
            tokio::spawn(async move {
                sqlx::query("INSERT INTO events VALUES(2, 'cancelled')")
                    .execute(&database)
                    .await
            })
        };
        tokio::task::yield_now().await;
        cancelled.abort();
        release.notify_one();
        first.await.unwrap().unwrap();
        sqlx::query("INSERT INTO events VALUES(3, 'after')")
            .execute(&database)
            .await
            .unwrap();
        let values: Vec<String> = sqlx::query_scalar("SELECT value FROM events ORDER BY sequence")
            .fetch_all(&database)
            .await
            .unwrap();
        assert_eq!(values, ["holder", "after"]);
    }

    #[tokio::test]
    async fn errors_and_abandoned_transactions_do_not_poison_the_writer() {
        let (_directory, database) = database().await;
        sqlx::query("INSERT INTO events VALUES(1, 'same')")
            .execute(&database)
            .await
            .unwrap();
        sqlx::query("INSERT INTO events VALUES(2, 'same')")
            .execute(&database)
            .await
            .unwrap_err();
        {
            let mut transaction = database.begin().await.unwrap();
            sqlx::query("INSERT INTO events VALUES(2, 'rolled back')")
                .execute(&mut *transaction)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO events VALUES(2, 'after')")
            .execute(&database)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn nested_writer_requests_fail_instead_of_deadlocking() {
        let (_directory, database) = database().await;
        let nested = database.clone();
        let error = database
            .write("outer", move |_connection| {
                Box::pin(async move {
                    nested
                        .write("inner", |_connection| Box::pin(async { Ok(()) }))
                        .await
                })
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nested SQLite writer request"));
    }
}
