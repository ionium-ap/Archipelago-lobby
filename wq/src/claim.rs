use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use redis::{FromRedisValue, ParsingError, ToRedisArgs, ToSingleRedisArg};
use serde::{Deserialize, Serialize};

use crate::{JobId, Priority};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Claim {
    pub job_id: JobId,
    pub priority: Priority,
    pub worker_id: String,
    pub time: DateTime<Utc>,
    /// The partition the job was claimed from, so that an expired claim puts it back where it
    /// came from. Absent for the default partition, which is also what a claim written before
    /// partitions existed looks like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<String>,
}

impl Claim {
    pub fn new(worker_id: &str, job_id: JobId, priority: Priority) -> Self {
        Self {
            job_id,
            priority,
            worker_id: worker_id.to_string(),
            time: Utc::now(),
            partition: None,
        }
    }

    pub fn in_partition(mut self, partition: Option<String>) -> Self {
        self.partition = partition;
        self
    }

    pub fn refresh(&mut self, worker_id: &str) -> Result<()> {
        if worker_id != self.worker_id {
            bail!(
                "Worker {} tried to refresh a claim that isn't theirs (owner is {})",
                worker_id,
                self.worker_id
            );
        }

        self.time = Utc::now();

        Ok(())
    }
}

impl ToRedisArgs for Claim {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let serialized = serde_json::to_string(self).expect("Failed to serialize claim");
        String::write_redis_args(&serialized, out)
    }
}

impl ToSingleRedisArg for Claim {}

impl FromRedisValue for Claim {
    fn from_redis_value(v: redis::Value) -> Result<Self, ParsingError> {
        let s = String::from_redis_value(v)?;
        let Ok(v) = serde_json::from_str(&s) else {
            return Err(ParsingError::from(format!(
                "Response was of incompatible type. Claim (response was {s:?})"
            )));
        };

        Ok(v)
    }
}
