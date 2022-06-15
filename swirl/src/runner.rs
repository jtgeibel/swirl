use diesel::prelude::*;
#[cfg(feature = "r2d2")]
use diesel::r2d2;
use std::any::Any;
use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe, PanicInfo, RefUnwindSafe, UnwindSafe};
use std::sync::Arc;
use std::time::Duration;
use threadpool::ThreadPool;

use crate::db::*;
use crate::errors::*;
use crate::{storage, Registry};
use event::*;

mod channel;
mod event;

pub struct NoConnectionPoolGiven;

#[allow(missing_debug_implementations)]
pub struct Builder<Env, ConnectionPoolBuilder> {
    connection_pool_or_builder: ConnectionPoolBuilder,
    environment: Env,
    thread_count: Option<usize>,
    job_start_timeout: Option<Duration>,
}

impl<Env, ConnectionPoolBuilder> Builder<Env, ConnectionPoolBuilder> {
    /// Set the number of threads to be used to run jobs concurrently.
    ///
    /// Defaults to 5
    pub fn thread_count(mut self, thread_count: usize) -> Self {
        self.thread_count = Some(thread_count);
        self
    }

    fn get_thread_count(&self) -> usize {
        self.thread_count.unwrap_or(5)
    }

    /// The amount of time to wait for a job to start before assuming an error
    /// has occurred.
    ///
    /// Defaults to 10 seconds.
    pub fn job_start_timeout(mut self, timeout: Duration) -> Self {
        self.job_start_timeout = Some(timeout);
        self
    }

    /// Provide a connection pool to be used by the runner
    pub fn connection_pool<NewPool>(self, pool: NewPool) -> Builder<Env, NewPool> {
        Builder {
            connection_pool_or_builder: pool,
            environment: self.environment,
            thread_count: self.thread_count,
            job_start_timeout: self.job_start_timeout,
        }
    }
}

#[cfg(feature = "r2d2")]
impl<Env, ConnectionPoolBuilder> Builder<Env, ConnectionPoolBuilder> {
    /// Build the runner with an r2d2 connection pool
    ///
    /// This will override any connection pool previously provided
    pub fn database_url<S: Into<String>>(self, database_url: S) -> Builder<Env, R2d2Builder> {
        self.connection_pool_builder(database_url, r2d2::Builder::new())
    }

    /// Provide a connection pool builder.
    ///
    /// This will override any connection pool previously provided.
    ///
    /// You should call this method if you want to provide additional
    /// configuration for the database connection pool. The builder will be
    /// configured to have its max size set to the value given to `2 * thread_count`.
    /// To override this behavior, call [`connection_count`](Self::connection_count)
    pub fn connection_pool_builder<S: Into<String>>(
        self,
        database_url: S,
        builder: r2d2::Builder<r2d2::ConnectionManager<PgConnection>>,
    ) -> Builder<Env, R2d2Builder> {
        self.connection_pool(R2d2Builder::new(database_url.into(), builder))
    }
}

#[cfg(feature = "r2d2")]
impl<Env> Builder<Env, R2d2Builder> {
    /// Set the max size of the database connection pool
    pub fn connection_count(mut self, connection_count: u32) -> Self {
        self.connection_pool_or_builder
            .connection_count(connection_count);
        self
    }

    /// Build the runner with an r2d2 connection pool.
    pub fn build(self) -> Runner<Env, r2d2::Pool<r2d2::ConnectionManager<PgConnection>>> {
        let thread_count = self.get_thread_count();
        let connection_pool_size = thread_count as u32 * 2;
        let connection_pool = self.connection_pool_or_builder.build(connection_pool_size);

        Runner {
            connection_pool,
            thread_pool: ThreadPool::new(thread_count),
            environment: Arc::new(self.environment),
            registry: Arc::new(Registry::load()),
            job_start_timeout: self.job_start_timeout.unwrap_or(Duration::from_secs(10)),
        }
    }
}

impl<Env, ConnectionPool> Builder<Env, ConnectionPool>
where
    ConnectionPool: DieselPool,
{
    /// Build the runner
    pub fn build(self) -> Runner<Env, ConnectionPool> {
        Runner {
            thread_pool: ThreadPool::new(self.get_thread_count()),
            connection_pool: self.connection_pool_or_builder,
            environment: Arc::new(self.environment),
            registry: Arc::new(Registry::load()),
            job_start_timeout: self.job_start_timeout.unwrap_or(Duration::from_secs(10)),
        }
    }
}

#[allow(missing_debug_implementations)]
/// The core runner responsible for locking and running jobs
pub struct Runner<Env: 'static, ConnectionPool> {
    connection_pool: ConnectionPool,
    thread_pool: ThreadPool,
    environment: Arc<Env>,
    registry: Arc<Registry<Env>>,
    job_start_timeout: Duration,
}

impl<Env> Runner<Env, NoConnectionPoolGiven> {
    /// Create a builder for a job runner
    ///
    /// This method takes the two required configurations, the database
    /// connection pool, and the environment to pass to your jobs. If your
    /// environment contains a connection pool, it should be the same pool given
    /// here.
    pub fn builder(environment: Env) -> Builder<Env, NoConnectionPoolGiven> {
        Builder {
            connection_pool_or_builder: NoConnectionPoolGiven,
            environment,
            thread_count: None,
            job_start_timeout: None,
        }
    }
}

impl<Env, ConnectionPool> Runner<Env, ConnectionPool> {
    #[doc(hidden)]
    /// For use in integration tests
    pub fn connection_pool(&self) -> &ConnectionPool {
        &self.connection_pool
    }
}

impl<Env, ConnectionPool> Runner<Env, ConnectionPool>
where
    Env: RefUnwindSafe + Send + Sync + 'static,
    ConnectionPool: DieselPool + 'static,
{
    /// Runs all pending jobs in the queue
    ///
    /// This function will return once all jobs in the queue have begun running,
    /// but does not wait for them to complete. When this function returns, at
    /// least one thread will have tried to acquire a new job, and found there
    /// were none in the queue.
    pub fn run_all_pending_jobs(&self) -> Result<(), FetchError<ConnectionPool>> {
        use std::cmp::max;

        let max_threads = self.thread_pool.max_count();
        let (sender, receiver) = channel::new(max_threads);
        let mut pending_messages = 0;
        loop {
            let available_threads = max_threads - self.thread_pool.active_count();

            let jobs_to_queue = if pending_messages == 0 {
                // If we have no queued jobs talking to us, and there are no
                // available threads, we still need to queue at least one job
                // or we'll never receive a message
                max(available_threads, 1)
            } else {
                available_threads
            };

            for _ in 0..jobs_to_queue {
                self.run_single_job(sender.clone());
            }

            pending_messages += jobs_to_queue;
            match receiver.recv_timeout(self.job_start_timeout) {
                Ok(Event::Working) => pending_messages -= 1,
                Ok(Event::NoJobAvailable) => return Ok(()),
                Ok(Event::ErrorLoadingJob(e)) => return Err(FetchError::FailedLoadingJob(e)),
                Ok(Event::FailedToAcquireConnection(e)) => {
                    return Err(FetchError::NoDatabaseConnection(e));
                }
                Err(_) => return Err(FetchError::NoMessageReceived),
            }
        }
    }

    fn run_single_job(&self, sender: EventSender<ConnectionPool>) {
        let environment = Arc::clone(&self.environment);
        let registry = Arc::clone(&self.registry);
        // FIXME: https://github.com/sfackler/r2d2/pull/70
        let connection_pool = AssertUnwindSafe(self.connection_pool().clone());
        self.get_single_job(sender, move |job| {
            let perform_job = registry
                .get(&job.job_type)
                .ok_or_else(|| PerformError::from(format!("Unknown job type {}", job.job_type)))?;
            perform_job.perform(job.data, &environment, &connection_pool.0)
        })
    }

    fn get_single_job<F>(&self, sender: EventSender<ConnectionPool>, f: F)
    where
        F: FnOnce(storage::BackgroundJob) -> Result<(), PerformError> + Send + UnwindSafe + 'static,
    {
        use diesel::result::Error::RollbackTransaction;

        // The connection may not be `Send` so we need to clone the pool instead
        let pool = self.connection_pool.clone();
        self.thread_pool.execute(move || {
            let conn = &mut *match pool.get() {
                Ok(conn) => conn,
                Err(e) => {
                    sender.send(Event::FailedToAcquireConnection(e));
                    return;
                }
            };

            let job_run_result = conn.transaction::<_, diesel::result::Error, _>(|conn| {
                let job = match storage::find_next_unlocked_job(conn).optional() {
                    Ok(Some(j)) => {
                        sender.send(Event::Working);
                        j
                    }
                    Ok(None) => {
                        sender.send(Event::NoJobAvailable);
                        return Ok(());
                    }
                    Err(e) => {
                        sender.send(Event::ErrorLoadingJob(e));
                        return Err(RollbackTransaction);
                    }
                };
                let job_id = job.id;

                let result = catch_unwind(|| f(job))
                    .map_err(|e| try_to_extract_panic_info(&e))
                    .and_then(|r| r);

                match result {
                    Ok(_) => storage::delete_successful_job(conn, job_id)?,
                    Err(e) => {
                        eprintln!("Job {} failed to run: {}", job_id, e);
                        storage::update_failed_job(conn, job_id);
                    }
                }
                Ok(())
            });

            match job_run_result {
                Ok(_) | Err(RollbackTransaction) => {}
                Err(e) => {
                    panic!("Failed to update job: {:?}", e);
                }
            }
        })
    }

    fn connection(&self) -> Result<DieselPooledConn<ConnectionPool>, Box<dyn Error + Send + Sync>> {
        self.connection_pool.get().map_err(Into::into)
    }

    /// Waits for all running jobs to complete, and returns an error if any
    /// failed
    ///
    /// This function is intended for use in tests. If any jobs have failed, it
    /// will return `swirl::JobsFailed` with the number of jobs that failed.
    ///
    /// If any other unexpected errors occurred, such as panicked worker threads
    /// or an error loading the job count from the database, an opaque error
    /// will be returned.
    pub fn check_for_failed_jobs(&self) -> Result<(), FailedJobsError> {
        self.wait_for_jobs()?;
        let failed_jobs = storage::failed_job_count(&mut *self.connection()?)?;
        if failed_jobs == 0 {
            Ok(())
        } else {
            Err(JobsFailed(failed_jobs))
        }
    }

    fn wait_for_jobs(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.thread_pool.join();
        let panic_count = self.thread_pool.panic_count();
        if panic_count == 0 {
            Ok(())
        } else {
            Err(format!("{} threads panicked", panic_count).into())
        }
    }
}

/// Try to figure out what's in the box, and print it if we can.
///
/// The actual error type we will get from `panic::catch_unwind` is really poorly documented.
/// However, the `panic::set_hook` functions deal with a `PanicInfo` type, and its payload is
/// documented as "commonly but not always `&'static str` or `String`". So we can try all of those,
/// and give up if we didn't get one of those three types.
fn try_to_extract_panic_info(info: &(dyn Any + Send + 'static)) -> PerformError {
    if let Some(x) = info.downcast_ref::<PanicInfo>() {
        format!("job panicked: {}", x).into()
    } else if let Some(x) = info.downcast_ref::<&'static str>() {
        format!("job panicked: {}", x).into()
    } else if let Some(x) = info.downcast_ref::<String>() {
        format!("job panicked: {}", x).into()
    } else {
        "job panicked".into()
    }
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;
    use diesel::r2d2;

    use super::*;
    use crate::schema::background_jobs::dsl::*;
    use std::panic::AssertUnwindSafe;
    use std::sync::{Arc, Barrier, Mutex, MutexGuard};

    #[test]
    fn jobs_are_locked_when_fetched() {
        let _guard = TestGuard::lock();

        let runner = runner();
        let first_job_id = create_dummy_job(&runner).id;
        let second_job_id = create_dummy_job(&runner).id;
        let fetch_barrier = Arc::new(AssertUnwindSafe(Barrier::new(2)));
        let fetch_barrier2 = fetch_barrier.clone();
        let return_barrier = Arc::new(AssertUnwindSafe(Barrier::new(2)));
        let return_barrier2 = return_barrier.clone();

        runner.get_single_job(channel::dummy_sender(), move |job| {
            fetch_barrier.0.wait(); // Tell thread 2 it can lock its job
            assert_eq!(first_job_id, job.id);
            return_barrier.0.wait(); // Wait for thread 2 to lock its job
            Ok(())
        });

        fetch_barrier2.0.wait(); // Wait until thread 1 locks its job
        runner.get_single_job(channel::dummy_sender(), move |job| {
            assert_eq!(second_job_id, job.id);
            return_barrier2.0.wait(); // Tell thread 1 it can unlock its job
            Ok(())
        });

        runner.wait_for_jobs().unwrap();
    }

    #[test]
    fn jobs_are_deleted_when_successfully_run() {
        let _guard = TestGuard::lock();

        let runner = runner();
        create_dummy_job(&runner);

        runner.get_single_job(channel::dummy_sender(), |_| Ok(()));
        runner.wait_for_jobs().unwrap();

        let remaining_jobs = background_jobs
            .count()
            .get_result(&mut *runner.connection().unwrap());
        assert_eq!(Ok(0), remaining_jobs);
    }

    #[test]
    fn failed_jobs_do_not_release_lock_before_updating_retry_time() {
        let _guard = TestGuard::lock();

        let runner = runner();
        create_dummy_job(&runner);
        let barrier = Arc::new(AssertUnwindSafe(Barrier::new(2)));
        let barrier2 = barrier.clone();

        runner.get_single_job(channel::dummy_sender(), move |_| {
            barrier.0.wait();
            // error so the job goes back into the queue
            Err("nope".into())
        });

        let conn = &mut runner.connection().unwrap();
        // Wait for the first thread to acquire the lock
        barrier2.0.wait();
        // We are intentionally not using `get_single_job` here.
        // `SKIP LOCKED` is intentionally omitted here, so we block until
        // the lock on the first job is released.
        // If there is any point where the row is unlocked, but the retry
        // count is not updated, we will get a row here.
        let available_jobs = background_jobs
            .select(id)
            .filter(retries.eq(0))
            .for_update()
            .load::<i64>(conn)
            .unwrap();
        assert_eq!(0, available_jobs.len());

        // Sanity check to make sure the job actually is there
        let total_jobs_including_failed = background_jobs
            .select(id)
            .for_update()
            .load::<i64>(conn)
            .unwrap();
        assert_eq!(1, total_jobs_including_failed.len());

        runner.wait_for_jobs().unwrap();
    }

    #[test]
    fn panicking_in_jobs_updates_retry_counter() {
        let _guard = TestGuard::lock();
        let runner = runner();
        let job_id = create_dummy_job(&runner).id;

        runner.get_single_job(channel::dummy_sender(), |_| panic!());
        runner.wait_for_jobs().unwrap();

        let tries = background_jobs
            .find(job_id)
            .select(retries)
            .for_update()
            .first::<i32>(&mut *runner.connection().unwrap())
            .unwrap();
        assert_eq!(1, tries);
    }

    lazy_static::lazy_static! {
        // Since these tests deal with behavior concerning multiple connections
        // running concurrently, they have to run outside of a transaction.
        // Therefore we can't run more than one at a time.
        //
        // Rather than forcing the whole suite to be run with `--test-threads 1`,
        // we just lock these tests instead.
        static ref TEST_MUTEX: Mutex<()> = Mutex::new(());
    }

    struct TestGuard<'a>(MutexGuard<'a, ()>);

    impl<'a> TestGuard<'a> {
        fn lock() -> Self {
            TestGuard(TEST_MUTEX.lock().unwrap())
        }
    }

    impl<'a> Drop for TestGuard<'a> {
        fn drop(&mut self) {
            ::diesel::sql_query("TRUNCATE TABLE background_jobs")
                .execute(&mut *runner().connection().unwrap())
                .unwrap();
        }
    }

    type Runner<Env> = crate::Runner<Env, r2d2::Pool<r2d2::ConnectionManager<PgConnection>>>;

    fn runner() -> Runner<()> {
        let database_url =
            dotenv::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set to run tests");

        crate::Runner::builder(())
            .database_url(database_url)
            .thread_count(2)
            .build()
    }

    fn create_dummy_job(runner: &Runner<()>) -> storage::BackgroundJob {
        ::diesel::insert_into(background_jobs)
            .values((job_type.eq("Foo"), data.eq(serde_json::json!(null))))
            .returning((id, job_type, data))
            .get_result(&mut *runner.connection().unwrap())
            .unwrap()
    }
}
