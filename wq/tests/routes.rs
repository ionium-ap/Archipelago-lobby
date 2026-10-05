#![cfg(feature = "rocket")]

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use rocket::{
    http::{Header, Status},
    local::asynchronous::Client,
};
use serde_json::json;
use uuid::Uuid;
use wq::{rocket_routes::QueueTokens, Job, JobStatus, Priority, WorkQueue};

mod common;
use common::{start_valkey, TestWork, TestWorkResult, ValkeyInstance, DEFAULT_DEADLINE};

mod queues {
    use super::common::{TestWork, TestWorkResult};

    wq::declare_queues!(test_queue<TestWork, TestWorkResult>);
}

type TestQueue = WorkQueue<TestWork, TestWorkResult>;

async fn build_queue(valkey: &ValkeyInstance) -> Result<TestQueue> {
    TestQueue::builder("routes")
        .with_default_partition("0.6.7")
        .with_claim_timeout(Duration::from_millis(100))
        .build(&valkey.url())
        .await
}

/// A lobby serving the queue, and a second handle on the same queue to enqueue with.
async fn lobby(valkey: &ValkeyInstance) -> Result<(Client, TestQueue)> {
    let tokens = QueueTokens(HashMap::from([("test_queue", "token".to_string())]));
    let rocket = rocket::build()
        .manage(build_queue(valkey).await?)
        .manage(tokens)
        .mount("/queues", queues::routes());

    let client = Client::untracked(rocket)
        .await
        .expect("The test lobby should start");

    Ok((client, build_queue(valkey).await?))
}

fn work() -> TestWork {
    TestWork(Uuid::new_v4().to_string())
}

async fn claim(client: &Client, body: serde_json::Value) -> Option<Job<TestWork>> {
    let response = client
        .post("/queues/test_queue/claim_job")
        .header(Header::new("X-Worker-Auth", "token"))
        .json(&body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);

    response.into_json().await.expect("A JSON body")
}

#[rocket::async_test]
async fn test_claim_route_gives_a_worker_its_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let (client, queue) = lobby(&valkey).await?;

    let old_job = queue
        .enqueue_job(&work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let new_job = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;

    // What a worker for the newer version sends
    let job = claim(&client, json!({"worker_id": "new", "partition": "0.6.8"})).await;
    assert_eq!(job.map(|job| job.job_id), Some(new_job));
    let job = claim(&client, json!({"worker_id": "new", "partition": "0.6.8"})).await;
    assert!(job.is_none());

    // What a worker from before partitions sends
    let job = claim(&client, json!({"worker_id": "old"})).await;
    assert_eq!(job.map(|job| job.job_id), Some(old_job));

    // And a current worker for the default partition
    let old_job = queue
        .enqueue_job(&work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let job = claim(&client, json!({"worker_id": "old", "partition": "0.6.7"})).await;
    assert_eq!(job.map(|job| job.job_id), Some(old_job));

    Ok(())
}

#[rocket::async_test]
async fn test_reclaim_and_resolve_routes_accept_a_partition() -> Result<()> {
    let valkey = start_valkey()?;
    let (client, queue) = lobby(&valkey).await?;

    let job_id = queue
        .enqueue_job_in(Some("0.6.8"), &work(), Priority::Low, DEFAULT_DEADLINE)
        .await?;
    let job = claim(&client, json!({"worker_id": "new", "partition": "0.6.8"})).await;
    assert_eq!(job.map(|job| job.job_id), Some(job_id));

    let response = client
        .post("/queues/test_queue/reclaim_job")
        .header(Header::new("X-Worker-Auth", "token"))
        .json(&json!({"worker_id": "new", "job_id": job_id, "partition": "0.6.8"}))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);

    let response = client
        .post("/queues/test_queue/resolve_job")
        .header(Header::new("X-Worker-Auth", "token"))
        .json(&json!({
            "worker_id": "new",
            "job_id": job_id,
            "status": "Success",
            "result": "done",
            "partition": "0.6.8",
        }))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        queue.get_job_status(&job_id).await?,
        Some(JobStatus::Success)
    );

    Ok(())
}

#[rocket::async_test]
async fn test_claim_route_refuses_a_bad_partition_name() -> Result<()> {
    let valkey = start_valkey()?;
    let (client, _queue) = lobby(&valkey).await?;

    let response = client
        .post("/queues/test_queue/claim_job")
        .header(Header::new("X-Worker-Auth", "token"))
        .json(&json!({"worker_id": "worker", "partition": "0.6.8:queue"}))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);

    Ok(())
}

#[rocket::async_test]
async fn test_claim_route_needs_the_token() -> Result<()> {
    let valkey = start_valkey()?;
    let (client, _queue) = lobby(&valkey).await?;

    let response = client
        .post("/queues/test_queue/claim_job")
        .json(&json!({"worker_id": "worker", "partition": "0.6.8"}))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);

    Ok(())
}
