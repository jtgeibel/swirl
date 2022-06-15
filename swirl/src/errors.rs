use diesel::result::Error as DieselError;
use std::error::Error;
use std::fmt;

use crate::db::DieselPool;

/// An error occurred queueing the job
#[derive(Debug)]
#[non_exhaustive]
pub enum EnqueueError {
    /// An error occurred serializing the job
    SerializationError(serde_json::error::Error),

    /// An error occurred inserting the job into the database
    DatabaseError(DieselError),
}

impl From<serde_json::error::Error> for EnqueueError {
    fn from(e: serde_json::error::Error) -> Self {
        EnqueueError::SerializationError(e)
    }
}

impl From<DieselError> for EnqueueError {
    fn from(e: DieselError) -> Self {
        EnqueueError::DatabaseError(e)
    }
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EnqueueError::SerializationError(e) => e.fmt(f),
            EnqueueError::DatabaseError(e) => e.fmt(f),
        }
    }
}

impl Error for EnqueueError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            EnqueueError::SerializationError(e) => Some(e),
            EnqueueError::DatabaseError(e) => Some(e),
        }
    }
}

/// An error occurred performing the job
pub type PerformError = Box<dyn Error>;

/// An error occurred while attempting to fetch jobs from the queue
pub enum FetchError<Pool: DieselPool> {
    /// We could not acquire a database connection from the pool.
    ///
    /// Either the connection pool is too small, or new connections cannot be
    /// established.
    NoDatabaseConnection(Pool::Error),

    /// Could not execute the query to load a job from the database.
    FailedLoadingJob(DieselError),

    /// No message was received from the worker thread.
    ///
    /// Either the thread pool is too small, or jobs have hung indefinitely
    NoMessageReceived,
}

impl<Pool: DieselPool> fmt::Debug for FetchError<Pool> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FetchError::NoDatabaseConnection(e) => {
                f.debug_tuple("NoDatabaseConnection").field(e).finish()
            }
            FetchError::FailedLoadingJob(e) => f.debug_tuple("FailedLoadingJob").field(e).finish(),
            FetchError::NoMessageReceived => f.debug_struct("NoMessageReceived").finish(),
        }
    }
}

impl<Pool: DieselPool> fmt::Display for FetchError<Pool> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FetchError::NoDatabaseConnection(e) => {
                write!(f, "Timed out acquiring a database connection. ")?;
                write!(f, "Try increasing the connection pool size: ")?;
                write!(f, "{}", e)?;
            }
            FetchError::FailedLoadingJob(e) => {
                write!(f, "An error occurred loading a job from the database: ")?;
                write!(f, "{}", e)?;
            }
            FetchError::NoMessageReceived => {
                write!(f, "No message was received from the worker thread. ")?;
                write!(f, "Try increasing the thread pool size or timeout period.")?;
            }
        }
        Ok(())
    }
}

impl<Pool: DieselPool> Error for FetchError<Pool> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            FetchError::NoDatabaseConnection(e) => Some(e),
            FetchError::FailedLoadingJob(e) => Some(e),
            FetchError::NoMessageReceived => None,
        }
    }
}

/// An error returned by `Runner::check_for_failed_jobs`. Only used in tests.
#[derive(Debug)]
pub enum FailedJobsError {
    /// Jobs failed to run
    JobsFailed(
        /// The number of failed jobs
        i64,
    ),

    #[doc(hidden)]
    /// Match on `_` instead, more variants may be added in the future
    /// Some other error occurred. Worker threads may have panicked, an error
    /// occurred counting failed jobs in the DB, or something else
    /// unexpectedly went wrong.
    __Unknown(Box<dyn Error + Send + Sync>),
}

pub use FailedJobsError::JobsFailed;

impl From<Box<dyn Error + Send + Sync>> for FailedJobsError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        FailedJobsError::__Unknown(e)
    }
}

impl From<DieselError> for FailedJobsError {
    fn from(e: DieselError) -> Self {
        FailedJobsError::__Unknown(e.into())
    }
}

impl PartialEq for FailedJobsError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (JobsFailed(x), JobsFailed(y)) => x == y,
            _ => false,
        }
    }
}

impl fmt::Display for FailedJobsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use FailedJobsError::*;

        match self {
            JobsFailed(x) => write!(f, "{} jobs failed", x),
            FailedJobsError::__Unknown(e) => e.fmt(f),
        }
    }
}

impl Error for FailedJobsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            JobsFailed(_) => None,
            FailedJobsError::__Unknown(e) => Some(&**e),
        }
    }
}
