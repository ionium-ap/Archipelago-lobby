use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::Result;
use redis::Commands;
use uuid::Uuid;

mod common;
use common::{start_valkey, TestWork, TestWorkResult, ValkeyInstance, DEFAULT_DEADLINE};
use wq::{Claim, JobDesc, JobId, JobResult, JobStatus, Priority, WorkQueue, WorkQueueError};

type TestQueue = WorkQueue<TestWork, TestWorkResult>;

async fn partitioned_queue(valkey: &ValkeyInstance, name: &str) -> Result<TestQueue> {
    TestQueue::builder(name)
        .with_default_partition("0.6.7")
        .with_claim_timeout(Duration::from_millis(100))
        .build(&valkey.url())
        .await
}

fn work() -> TestWork {
    TestWork(Uuid::new_v4().to_string())
}

#[tokio::test]
async fn test_worker_only_gets_jobs_of_its_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "partitions").await?;

    let old_job = queue
        .enqueue_job_in(Some("0.6.7"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let new_job = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;

    let claimed = queue.claim_job_in(Some("0.6.8"), "new").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(new_job));
    assert!(queue.claim_job_in(Some("0.6.8"), "new").await?.is_none());

    let claimed = queue.claim_job_in(Some("0.6.7"), "old").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(old_job));
    assert!(queue.claim_job_in(Some("0.6.7"), "old").await?.is_none());

    Ok(())
}

#[tokio::test]
async fn test_no_partition_is_the_default_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "default_partition").await?;

    // Enqueued without a partition, claimed by a worker that names the default one
    let unnamed = queue
        .enqueue_job(&work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let claimed = queue.claim_job_in(Some("0.6.7"), "named").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(unnamed));

    // And the other way around, which is a worker from before partitions existed
    let named = queue
        .enqueue_job_in(Some("0.6.7"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let claimed = queue.claim_job("unnamed").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(named));

    // A worker that names no partition never gets another partition's jobs
    queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    assert!(queue.claim_job("unnamed").await?.is_none());

    Ok(())
}

#[tokio::test]
async fn test_default_partition_keeps_the_original_keys() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "legacy_keys").await?;
    let mut redis = redis::Client::open(valkey.url())?;

    let old_job = queue
        .enqueue_job_in(Some("0.6.7"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let new_job = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;

    let pending: Vec<String> = redis.zrange("wq:legacy_keys:queue", 0, -1)?;
    assert_eq!(pending, vec![old_job.to_string()]);
    let pending: Vec<String> = redis.zrange("wq:legacy_keys:partition:0.6.8:queue", 0, -1)?;
    assert_eq!(pending, vec![new_job.to_string()]);

    // Both descriptions are where a job's description always was, and the default partition's
    // one is what it always was: nothing in it names a partition.
    let old_desc: String = redis.get(format!("wq:legacy_keys:queue:{old_job}"))?;
    assert!(!old_desc.contains("partition"));
    let new_desc: String = redis.get(format!("wq:legacy_keys:queue:{new_job}"))?;
    let new_desc: JobDesc<TestWork> = serde_json::from_str(&new_desc)?;
    assert_eq!(new_desc.partition.as_deref(), Some("0.6.8"));

    // Same for the claims
    queue.claim_job_in(Some("0.6.7"), "old").await?;
    queue.claim_job_in(Some("0.6.8"), "new").await?;
    let old_claim: String = redis.hget("wq:legacy_keys:claims", old_job.to_string())?;
    assert!(!old_claim.contains("partition"));
    let new_claim: Claim = redis.hget("wq:legacy_keys:claims", new_job.to_string())?;
    assert_eq!(new_claim.partition.as_deref(), Some("0.6.8"));

    Ok(())
}

#[tokio::test]
async fn test_job_enqueued_before_partitions_is_still_claimed() -> Result<()> {
    let valkey = start_valkey()?;
    let mut redis = redis::Client::open(valkey.url())?;

    // What a lobby from before partitions wrote, with a job pending and another one claimed
    let pending_job = JobId::new();
    let claimed_job = JobId::new();
    let desc = r#"{"params":"work","submitted_at":"2026-10-05T00:00:00Z","deadline":"2999-01-01T00:00:00Z"}"#;
    redis.set::<_, _, ()>(format!("wq:upgrade:queue:{pending_job}"), desc)?;
    redis.set::<_, _, ()>(format!("wq:upgrade:queue:{claimed_job}"), desc)?;
    redis.zadd::<_, _, _, ()>("wq:upgrade:queue", pending_job.to_string(), -1)?;
    redis.hset::<_, _, _, ()>(
        "wq:upgrade:claims",
        claimed_job.to_string(),
        format!(
            r#"{{"job_id":"{claimed_job}","priority":"Low","worker_id":"worker","time":"2026-10-05T00:00:00Z"}}"#
        ),
    )?;

    let queue = partitioned_queue(&valkey, "upgrade").await?;

    let claimed = queue.claim_job_in(Some("0.6.7"), "worker").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(pending_job));

    queue
        .resolve_job(
            "worker",
            claimed_job,
            JobStatus::Success,
            Some(TestWorkResult("done".to_string())),
        )
        .await?;
    assert_eq!(
        queue.get_job_status(&claimed_job).await?,
        Some(JobStatus::Success)
    );

    Ok(())
}

#[tokio::test]
async fn test_expired_claim_goes_back_to_its_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = TestQueue::builder("partition_reclaim")
        .with_default_partition("0.6.7")
        .with_claim_timeout(Duration::from_millis(100))
        .with_reclaim_timeout(Duration::from_millis(100))
        .build(&valkey.url())
        .await?;
    queue.start_reclaim_checker();

    let job_id = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let claimed = queue.claim_job_in(Some("0.6.8"), "gone").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(job_id));

    // The worker never reclaims. Its job must not land in the default partition.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(queue.claim_job_in(Some("0.6.7"), "old").await?.is_none());
    let claimed = queue.claim_job_in(Some("0.6.8"), "new").await?;
    assert_eq!(claimed.map(|job| job.job_id), Some(job_id));

    Ok(())
}

#[tokio::test]
async fn test_cancel_removes_the_job_from_its_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "partition_cancel").await?;

    let job_id = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    assert_eq!(queue.get_stats().await?.jobs_scheduled, 1);

    queue.cancel_job(job_id).await?;

    assert_eq!(queue.get_stats().await?.jobs_scheduled, 0);
    assert!(queue.claim_job_in(Some("0.6.8"), "new").await?.is_none());
    assert_eq!(queue.get_job_status(&job_id).await?, None);

    Ok(())
}

#[tokio::test]
async fn test_stats_count_every_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "partition_stats").await?;

    queue
        .enqueue_job(&work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    for _ in 0..2 {
        queue
            .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
            .await?;
    }
    assert_eq!(queue.get_stats().await?.jobs_scheduled, 3);

    let job = queue
        .claim_job_in(Some("0.6.8"), "new")
        .await?
        .expect("Should've gotten a job");
    let stats = queue.get_stats().await?;
    assert_eq!(stats.jobs_scheduled, 2);
    assert_eq!(stats.jobs_claimed, 1);

    queue
        .resolve_job("new", job.job_id, JobStatus::Success, None)
        .await?;
    let stats = queue.get_stats().await?;
    assert_eq!(stats.jobs_claimed, 0);
    assert_eq!(stats.jobs_succeeded, 1);

    Ok(())
}

#[tokio::test]
async fn test_invalid_partition_names_are_refused() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = partitioned_queue(&valkey, "partition_names").await?;

    let long_name = "1".repeat(65);
    for name in ["", "0.6.8:queue", "a b", "0.6.8\n", long_name.as_str()] {
        let error = queue
            .claim_job_in(Some(name), "worker")
            .await
            .expect_err("The name should be refused");
        assert!(
            matches!(
                error.downcast_ref::<WorkQueueError>(),
                Some(WorkQueueError::InvalidPartition(_))
            ),
            "{name:?} gave {error}"
        );
        assert!(queue
            .enqueue_job_in(Some(name), &work(), Priority::Low, DEFAULT_DEADLINE)
            .await
            .is_err());
    }

    // What a version can look like is fine
    for name in ["0.6.8", "0.7.0-rc1", "0.6.8+ionium.1"] {
        assert!(queue.claim_job_in(Some(name), "worker").await?.is_none());
    }

    Ok(())
}

fn cleaning_up_callback() -> wq::ResolveCallback<TestWork, TestWorkResult> {
    let callback = |_desc: JobDesc<TestWork>,
                    _result: JobResult<TestWorkResult>|
     -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> {
        Box::pin(async { Ok(true) })
    };

    Arc::pin(callback)
}

#[tokio::test]
async fn test_wait_for_job_that_was_already_cleaned_up() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = TestQueue::builder("wait_cleaned_up")
        .with_callback(cleaning_up_callback())
        .build(&valkey.url())
        .await?;

    let job_id = queue
        .enqueue_job(&work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let job = queue
        .claim_job("worker")
        .await?
        .expect("Should've gotten a job");
    queue
        .resolve_job("worker", job.job_id, JobStatus::Failure, None)
        .await?;

    // The callback took the result, so the job and its result are gone
    assert_eq!(queue.get_job_status(&job_id).await?, None);

    let status = queue
        .wait_for_job(&job_id, Some(Duration::from_secs(5)))
        .await?;
    assert_eq!(status, Some(JobStatus::Failure));

    Ok(())
}

#[tokio::test]
async fn test_wait_for_unknown_job_is_still_an_error() -> Result<()> {
    let valkey = start_valkey()?;
    let queue = TestQueue::builder("wait_unknown")
        .build(&valkey.url())
        .await?;

    let result = queue
        .wait_for_job(&JobId::new(), Some(Duration::from_secs(1)))
        .await;
    assert!(matches!(result, Err(WorkQueueError::JobNotFound)));

    Ok(())
}
