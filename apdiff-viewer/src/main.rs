use std::{borrow::Cow, collections::BTreeMap, ffi::OsStr, io::Cursor, path::PathBuf};

use apwm::changes::Checksum;
use askama::Template;
use askama_web::WebTemplate;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::AsyncPgConnection;
use futures::future::try_join_all;
use rocket::{
    http::{ContentType, Status},
    response::{self, Responder},
    routes, Request, Response, State,
};
use semver::Version;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};
use syntect::{
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};
use taskcluster::{ClientBuilder, Credentials, Index, Queue};

mod api;
mod apworld;
mod blob_store;
mod db;
mod diff;
mod guards;
mod schema;
mod source;
mod tc;

use diff::FileDiff;
use source::{ArtifactSource, BASE_MANUAL};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

pub fn get_syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

pub fn get_theme() -> &'static Theme {
    THEME.get_or_init(|| {
        let theme_file = Asset::get("github-dark.tmTheme")
            .expect("github-dark.tmTheme should be embedded in binary");
        let theme_xml = std::str::from_utf8(&theme_file.data)
            .expect("github-dark.tmTheme should be valid UTF-8");
        ThemeSet::load_from_reader(&mut std::io::Cursor::new(theme_xml))
            .expect("github-dark.tmTheme should be valid theme XML")
    })
}

#[derive(Debug)]
pub struct Error(pub anyhow::Error);
pub type Result<T> = std::result::Result<T, Error>;

impl Responder<'_, 'static> for Error {
    fn respond_to(self, _: &Request<'_>) -> response::Result<'static> {
        let error = self.0.to_string();
        Response::build()
            .status(Status::InternalServerError)
            .sized_body(error.len(), Cursor::new(error))
            .ok()
    }
}

impl<E> From<E> for Error
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Error(error.into())
    }
}

pub(crate) struct TcConfig {
    pub(crate) index_namespace_prefix: String,
}

pub(crate) struct TreeCache(pub(crate) Mutex<lru::LruCache<String, Arc<apworld::FileTree>>>);

/// Runtime config for the submission ingestion + render path.
pub(crate) struct SubmissionConfig {
    /// Externally-reachable base URL used to build the `url` field of the
    /// `POST /api/submissions` response.
    pub(crate) public_base_url: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct IndexPage {
    task_id: String,
    /// URL the from-version dropdown form should submit back to. Distinct
    /// from `task_id` so the same template serves both the TC `/<task_id>`
    /// route and the `/submission/<id>` route without confusing the form
    /// action with the legacy URL shape.
    self_path: String,
    css_version: &'static str,
    apworld_diffs: Vec<ApworldDiff>,
}

const CSS_VERSION: &str = std::env!("CSS_VERSION");

const BASE_MANUAL_PREFIX: &str = "base:";

#[derive(Debug)]
struct FromVersion {
    label: String,
    value: String,
}

#[derive(Debug)]
struct ApworldDiff {
    apworld_name: String,
    world_name: String,
    from_versions: Vec<FromVersion>,
    selected_from: Option<String>,
    versions: Vec<VersionDiff>,
}

#[derive(Debug)]
struct VersionDiff {
    version_range: String,
    version_id: String,
    files: Vec<FileDiff>,
}

#[derive(Template, WebTemplate)]
#[template(path = "tests.html")]
struct TestPage {
    results: TestResults,
}

#[derive(serde::Deserialize)]
struct TestResult {
    traceback: String,
    description: Option<String>,
}

#[derive(serde::Deserialize)]
struct UnexpectedSuccess {
    description: Option<String>,
}

#[derive(serde::Deserialize)]
struct TestResults {
    failures: BTreeMap<String, TestResult>,
    errors: BTreeMap<String, TestResult>,
    #[serde(default)]
    unexpected_successes: BTreeMap<String, UnexpectedSuccess>,
    #[serde(default)]
    expected_failures: BTreeMap<String, TestResult>,
    version: String,
    world_name: String,
}

fn deserialize_json<T: for<'de> serde::Deserialize<'de>>(text: &str) -> Result<T> {
    let mut deser = serde_json::Deserializer::from_str(text);
    Ok(serde_path_to_error::deserialize(&mut deser)?)
}

#[rocket::get("/<task_id>?<params..>")]
async fn get_task_diffs(
    task_id: &str,
    params: HashMap<String, String>,
    queue: &State<Option<Queue>>,
    index: &State<Option<Index>>,
    tc_config: &State<TcConfig>,
    tree_cache: &State<TreeCache>,
) -> Result<IndexPage> {
    let queue = queue
        .inner()
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Taskcluster integration is not configured on this deployment"))?;
    let index = index
        .inner()
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Taskcluster integration is not configured on this deployment"))?;
    let source = source::tc::TcSource::new(
        queue,
        index,
        &tc_config.index_namespace_prefix,
        task_id.to_string(),
        tree_cache.inner(),
    )
    .await?;
    render_diffs(&source, task_id, format!("/{task_id}"), &params).await
}

#[rocket::get("/submission/<id>?<params..>")]
async fn get_submission_diffs(
    id: &str,
    params: HashMap<String, String>,
    pool: &State<diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>>,
    blobs: &State<Arc<blob_store::BlobStore>>,
    tree_cache: &State<TreeCache>,
) -> Result<IndexPage> {
    let source = source::submission::SubmissionSource::load(
        pool.inner(),
        blobs.inner(),
        tree_cache.inner(),
        id,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("submission {id} not found"))?;
    render_diffs(&source, id, format!("/submission/{id}"), &params).await
}

async fn render_diffs(
    source: &dyn ArtifactSource,
    page_id: &str,
    self_path: String,
    params: &HashMap<String, String>,
) -> Result<IndexPage> {
    let changes = source.changes_json().await?;

    let apworld_diffs = try_join_all(
        changes
            .worlds
            .into_iter()
            .filter(|(_, wc)| {
                wc.added_versions
                    .iter()
                    .any(|v| !matches!(wc.checksums.get(v), Some(Checksum::Supported)))
            })
            .map(|(apworld_name, world_changes)| async move {
                let from_override = params.get(&format!("{apworld_name}_from"));
                process_world(
                    source,
                    &apworld_name,
                    world_changes,
                    from_override.map(|s| s.as_str()),
                )
                .await
            }),
    )
    .await?;

    Ok(IndexPage {
        task_id: page_id.to_string(),
        self_path,
        css_version: CSS_VERSION,
        apworld_diffs,
    })
}

async fn process_world(
    source: &dyn ArtifactSource,
    apworld_name: &str,
    world_changes: apwm::changes::WorldChanges,
    from_override: Option<&str>,
) -> Result<ApworldDiff> {
    let mut added_sorted = world_changes.added_versions.clone();
    added_sorted.retain(|v| !matches!(world_changes.checksums.get(v), Some(Checksum::Supported)));
    added_sorted.sort();

    let (indexed, to_trees) = futures::join!(
        async { source.list_versions(apworld_name).await.unwrap_or_default() },
        try_join_all(added_sorted.iter().map(|v| {
            let version = v.to_string();
            async move {
                let tree = source.fetch_tree(apworld_name, &version).await?;
                let annotations = source.annotations(apworld_name, &version).await?;
                Ok::<_, Error>((version, tree, annotations))
            }
        })),
    );
    let to_trees = to_trees?;

    let mut from_versions: Vec<FromVersion> = indexed
        .iter()
        .filter(|v| !world_changes.added_versions.contains(v))
        .map(|v| FromVersion {
            label: v.to_string(),
            value: v.to_string(),
        })
        .collect();

    let is_manual =
        apworld_name.to_lowercase().starts_with("manual_") && apworld_name != BASE_MANUAL;

    let base_indexed = if is_manual {
        let base = source.list_versions(BASE_MANUAL).await.unwrap_or_default();
        for v in &base {
            from_versions.push(FromVersion {
                label: format!("base manual {v}"),
                value: format!("{BASE_MANUAL_PREFIX}{v}"),
            });
        }
        base
    } else {
        Vec::new()
    };

    let latest_added = added_sorted
        .last()
        .map(|v| v.to_string())
        .unwrap_or_default();
    let selected_from = match from_override {
        Some("") => None,
        Some(v) => Some(v.to_string()),
        None => find_previous_version(&latest_added, &indexed)
            .or_else(|| base_indexed.last().map(|v| format!("{BASE_MANUAL_PREFIX}{v}"))),
    };

    let (selected_from, from_tree) = match &selected_from {
        Some(v) => {
            let is_base_manual = v.starts_with(BASE_MANUAL_PREFIX);
            let (resolve_name, resolve_version) =
                if let Some(base_v) = v.strip_prefix(BASE_MANUAL_PREFIX) {
                    (BASE_MANUAL, base_v)
                } else {
                    (apworld_name, v.as_str())
                };
            match source.fetch_tree(resolve_name, resolve_version).await {
                Ok(tree) => {
                    let tree = if is_base_manual {
                        Arc::new(apworld::rekey_tree(&tree, apworld_name))
                    } else {
                        tree
                    };
                    (selected_from, Some(tree))
                }
                Err(e) => {
                    tracing::warn!("Error fetching from version {v} for {apworld_name}: {e}");
                    (None, None)
                }
            }
        }
        None => (None, None),
    };

    let empty_tree = apworld::FileTree::new();
    let old_tree = from_tree.as_deref().unwrap_or(&empty_tree);

    let versions: Vec<VersionDiff> = to_trees
        .into_iter()
        .map(|(version, new_tree, annotations)| {
            let files = diff::compute::compute_file_tree_diff(old_tree, &new_tree, &annotations);
            let version_range = match &selected_from {
                Some(v) => format!("{v}...{version}"),
                None => format!("...{version}"),
            };
            VersionDiff {
                version_range,
                version_id: version,
                files,
            }
        })
        .collect();

    Ok(ApworldDiff {
        apworld_name: apworld_name.to_string(),
        world_name: world_changes.world_name,
        from_versions,
        selected_from,
        versions,
    })
}

fn find_previous_version(current: &str, indexed: &[Version]) -> Option<String> {
    let current_v = Version::parse(current).ok()?;
    indexed
        .iter()
        .filter(|&v| v < &current_v)
        .max()
        .map(|v| v.to_string())
}

#[rocket::get("/tests/<task_id>")]
async fn get_test_results(task_id: &str, queue: &State<Option<Queue>>) -> Result<TestPage> {
    let queue = queue
        .inner()
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Taskcluster integration is not configured on this deployment"))?;
    let artifacts = tc::get_task_artifacts(queue, task_id).await?;
    let Some(aptest_name) = artifacts
        .iter()
        .find(|path| path.starts_with("public/test_results/"))
    else {
        Err(anyhow::anyhow!(
            "This doesn't look like a supported task, it contains no test_results"
        ))?
    };

    let aptest_text = tc::fetch_artifact_text(queue, task_id, aptest_name).await?;
    let results: TestResults = deserialize_json(&aptest_text)?;

    Ok(TestPage { results })
}

#[derive(rust_embed::RustEmbed)]
#[folder = "./static/"]
struct Asset;

#[rocket::get("/static/<file..>")]
fn dist_static(file: PathBuf) -> Option<(ContentType, Cow<'static, [u8]>)> {
    let filename = file.display().to_string();
    let asset = Asset::get(&filename)?;
    let content_type = file
        .extension()
        .and_then(OsStr::to_str)
        .and_then(ContentType::from_extension)
        .unwrap_or(ContentType::Binary);

    Some((content_type, asset.data))
}

use diesel_migrations::{embed_migrations, EmbeddedMigrations};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations/");

#[rocket::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "debug");
    }
    env_logger::init();

    // Taskcluster is optional. If TASKCLUSTER_ROOT_URL isn't set, the TC-backed
    // routes (`GET /<task_id>`, `GET /tests/<task_id>`) error at request time
    // with a clear message; the submission routes work either way.
    let (queue, tc_index): (Option<Queue>, Option<Index>) =
        if let Ok(root_url) = std::env::var("TASKCLUSTER_ROOT_URL") {
            let mut client_builder = ClientBuilder::new(root_url);
            if let (Ok(client_id), Ok(access_token)) = (
                std::env::var("TASKCLUSTER_CLIENT_ID"),
                std::env::var("TASKCLUSTER_ACCESS_TOKEN"),
            ) {
                client_builder = client_builder.credentials(Credentials {
                    client_id,
                    access_token,
                    certificate: None,
                });
            }
            let q = Queue::new(client_builder.clone())?;
            let i = Index::new(client_builder)?;
            (Some(q), Some(i))
        } else {
            (None, None)
        };

    let db_url = std::env::var("DATABASE_URL")?;
    let db_pool: Pool<AsyncPgConnection> =
        common::db::get_database_pool(&db_url, MIGRATIONS).await?;

    let fuzz_api_key = guards::FuzzApiKeyConfig(std::env::var("FUZZ_API_KEY")?);

    let apdiff_api_key = guards::ApdiffApiKeyConfig(std::env::var("APDIFF_API_KEY")?);

    let storage_root = PathBuf::from(
        std::env::var("APDIFF_STORAGE_ROOT").unwrap_or_else(|_| "./apdiff-blobs".into()),
    );
    let blob_store = Arc::new(blob_store::BlobStore::new(storage_root));

    let submission_config = SubmissionConfig {
        public_base_url: std::env::var("APDIFF_PUBLIC_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8000".into()),
    };

    let tc_config = TcConfig {
        index_namespace_prefix: std::env::var("APWORLD_INDEX_NAMESPACE")
            .unwrap_or_else(|_| "ap.index.world".into()),
    };

    let tree_cache = TreeCache(Mutex::new(lru::LruCache::new(
        NonZeroUsize::new(32).unwrap(),
    )));

    rocket::build()
        .manage(queue)
        .manage(tc_index)
        .manage(tc_config)
        .manage(tree_cache)
        .manage(db_pool)
        .manage(fuzz_api_key)
        .manage(apdiff_api_key)
        .manage(blob_store)
        .manage(submission_config)
        .mount(
            "/",
            routes![
                get_task_diffs,
                get_submission_diffs,
                dist_static,
                get_test_results
            ],
        )
        .mount("/api", api::routes())
        .launch()
        .await
        .map_err(|e| anyhow::anyhow!("Rocket launch failed: {}", e))?;

    Ok(())
}
