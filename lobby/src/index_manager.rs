use anyhow::{anyhow, bail, Context, Result};
use deadpool_redis::Pool as RedisPool;
use http::header::CONTENT_DISPOSITION;
use redis::AsyncCommands;
use rocket::http::Header;
use semver::Version;
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio::task::JoinHandle;

use apwm::{Index, IndexSet, Manifest};
use git2::{Oid, Repository, ResetType};

use crate::utils::ZipFile;

/// Holds the commit of the index repository that every lobby process should have loaded, and
/// is the channel that says it changed.
const ANNOUNCED_INDEX: &str = "index:announced";

/// How long a process can go without looking at `ANNOUNCED_INDEX` when no message tells it
/// to. A process that missed the message is that far behind at most.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(30);

/// Where the index comes from and where its apworlds are kept
#[derive(Clone, Debug)]
pub struct IndexSource {
    pub repo_url: String,
    pub repo_branch: String,
    /// This process's own checkout of the index repository
    pub index_path: PathBuf,
    /// The downloaded apworlds. Can be shared with other lobby processes.
    pub apworlds_path: PathBuf,
}

impl IndexSource {
    pub fn from_env() -> Self {
        Self {
            repo_url: std::env::var("APWORLDS_INDEX_REPO_URL")
                .expect("Provide a `APWORLDS_INDEX_REPO_URL` env variable"),
            repo_branch: std::env::var("APWORLDS_INDEX_REPO_BRANCH")
                .expect("Provide a `APWORLDS_INDEX_REPO_BRANCH` env variable"),
            index_path: PathBuf::from(
                std::env::var("APWORLDS_INDEX_DIR").unwrap_or_else(|_| "./index".into()),
            ),
            apworlds_path: PathBuf::from(
                std::env::var("APWORLDS_PATH").expect("Provide a `APWORLDS_PATH` env variable"),
            ),
        }
    }
}

/// Which of the Archipelago versions the index describes this installation offers: the bases
/// a room can be made on and the options pages can be asked for. An operator enables a base
/// when workers for it are running; the index describing it isn't enough.
///
/// A base can be enabled before the index describes it. It is offered from the moment the
/// index does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BaseConfig {
    /// `AP_BASES`. `None` offers the index's own `archipelago_version` and nothing else, which
    /// is what a lobby did before it could be configured.
    enabled: Option<Vec<Version>>,
    /// `AP_DEFAULT_BASE`. `None` is the newest base on offer.
    default: Option<Version>,
}

impl BaseConfig {
    pub fn from_env() -> Result<Self> {
        let read = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
        };

        Self::parse(
            read("AP_BASES")?.as_deref(),
            read("AP_DEFAULT_BASE")?.as_deref(),
        )
    }

    /// `ap_bases` is a comma separated list of versions, `ap_default_base` one of them. Both
    /// are optional, and an empty value is the same as none.
    pub fn parse(ap_bases: Option<&str>, ap_default_base: Option<&str>) -> Result<Self> {
        let parse_version = |name: &str, value: &str| {
            Version::parse(value)
                .with_context(|| format!("{name}: {value:?} isn't an Archipelago version"))
        };

        let enabled = match ap_bases.map(str::trim).filter(|value| !value.is_empty()) {
            Some(ap_bases) => {
                let mut enabled = Vec::new();
                for base in ap_bases.split(',').map(str::trim) {
                    let base = parse_version("AP_BASES", base)?;
                    if !enabled.contains(&base) {
                        enabled.push(base);
                    }
                }
                Some(enabled)
            }
            None => None,
        };

        let default = ap_default_base
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| parse_version("AP_DEFAULT_BASE", value))
            .transpose()?;
        if let (Some(enabled), Some(default)) = (&enabled, &default) {
            if !enabled.contains(default) {
                bail!("AP_DEFAULT_BASE is {default}, which isn't one of AP_BASES");
            }
        }

        Ok(Self { enabled, default })
    }

    /// The bases on offer with this index, newest first
    fn offered(&self, index: &IndexSet) -> Vec<Version> {
        let mut offered: Vec<Version> = match &self.enabled {
            Some(enabled) => enabled
                .iter()
                .filter(|base| index.get(base).is_some())
                .cloned()
                .collect(),
            None => vec![index.legacy_base.clone()],
        };
        offered.sort_by(|a, b| b.cmp(a));

        offered
    }

    /// The base of whatever doesn't name one. When nothing is on offer, which takes a
    /// configuration that names no base of the index, it is the index's own version.
    fn default_base(&self, index: &IndexSet) -> Version {
        let offered = self.offered(index);
        match &self.default {
            Some(default) if offered.contains(default) => default.clone(),
            _ => offered
                .into_iter()
                .next()
                .unwrap_or_else(|| index.legacy_base.clone()),
        }
    }

    /// What can't be told before the index is known, and what an operator should hear about
    fn check(&self, index: &IndexSet) -> Result<()> {
        if let (None, Some(default)) = (&self.enabled, &self.default) {
            if default != &index.legacy_base {
                bail!(
                    "AP_DEFAULT_BASE is {default}, but without AP_BASES only Archipelago {} is offered",
                    index.legacy_base
                );
            }
        }

        for base in self.enabled.iter().flatten() {
            if index.get(base).is_none() {
                tracing::warn!(
                    "AP_BASES has Archipelago {base}, which the index doesn't describe. It isn't offered until it does."
                );
            }
        }
        if self.offered(index).is_empty() {
            tracing::error!("The index describes none of the Archipelago versions in AP_BASES");
        }

        Ok(())
    }

    /// One line for the log
    fn describe(&self, index: &IndexSet) -> String {
        let default = self.default_base(index);
        let offered: Vec<String> = self
            .offered(index)
            .into_iter()
            .map(|base| {
                if base == default {
                    format!("{base} (default)")
                } else {
                    base.to_string()
                }
            })
            .collect();

        if offered.is_empty() {
            "offering no Archipelago version".to_string()
        } else {
            format!("offering Archipelago {}", offered.join(", "))
        }
    }
}

/// The index as it is loaded, and the commit of its repository it was read from
struct Loaded {
    index: IndexSet,
    commit: String,
}

/// The index, as every Archipelago version it describes sees it. A room is on one of those
/// versions, its base, and everything about the room goes through that base's view: which
/// worlds exist, which releases they have, what its manifest resolves to.
///
/// Each lobby process has its own, and they are kept on the same commit of the index
/// repository through Valkey: a process that loads a new commit announces it, the others load
/// what was announced. See `announce` and `follow_once`.
///
/// Clones share one index.
#[derive(Clone)]
pub struct IndexManager {
    loaded: Arc<RwLock<Loaded>>,
    /// One load at a time: they all work in the same checkout
    load_lock: Arc<tokio::sync::Mutex<()>>,
    source: IndexSource,
    bases: BaseConfig,
    pub apworlds_path: PathBuf,
    redis_pool: RedisPool,
    redis_client: redis::Client,
    /// The last announced commit this process acted on, or announced itself
    seen_announcement: Arc<Mutex<Option<String>>>,
}

impl IndexManager {
    /// Fetches the index repository and reads the index, without downloading any apworld
    pub fn new(
        source: IndexSource,
        bases: BaseConfig,
        redis_pool: RedisPool,
        valkey_url: &str,
    ) -> Result<Self> {
        let (commit, _) = fetch_and_reset(&source, None)?;
        let index = IndexSet::new(&source.index_path.join("index.toml"))?;
        bases.check(&index)?;
        tracing::info!(
            "Read the index at commit {commit}, {}",
            bases.describe(&index)
        );

        let manager = Self {
            loaded: Arc::new(RwLock::new(Loaded { index, commit })),
            load_lock: Arc::new(tokio::sync::Mutex::new(())),
            apworlds_path: source.apworlds_path.clone(),
            source,
            bases,
            redis_pool,
            redis_client: redis::Client::open(valkey_url)?,
            seen_announcement: Arc::new(Mutex::new(None)),
        };

        Ok(manager)
    }

    /// Loads the newest commit of the index repository's branch, and downloads the apworlds
    /// it needs. Returns that commit. The other lobby processes only learn about it from
    /// `announce`.
    pub async fn update(&self) -> Result<String> {
        self.load(None).await
    }

    /// Fetches, resets the checkout to `wanted` or to the branch if there's no such commit,
    /// downloads, and swaps the index in. Returns the commit that got loaded.
    async fn load(&self, wanted: Option<&str>) -> Result<String> {
        let _guard = self.load_lock.lock().await;

        let (commit, _) = fetch_and_reset(&self.source, wanted)?;
        let new_index = IndexSet::new(&self.source.index_path.join("index.toml"))?;
        new_index
            .refresh_into(&self.apworlds_path, false, None)
            .await?;
        let offer = self.bases.describe(&new_index);
        *self.loaded.write().await = Loaded {
            index: new_index,
            commit: commit.clone(),
        };
        tracing::info!("Loaded the index at commit {commit}, {offer}");

        Ok(commit)
    }

    /// The commit of the index repository that is loaded
    pub async fn commit(&self) -> String {
        self.loaded.read().await.commit.clone()
    }

    async fn announced(&self) -> Result<Option<String>> {
        let mut conn = self.redis_pool.get().await?;

        Ok(conn.get::<_, Option<String>>(ANNOUNCED_INDEX).await?)
    }

    fn mark_seen(&self, commit: &str) {
        *self.seen_announcement.lock().unwrap() = Some(commit.to_string());
    }

    /// Tells every lobby process to load the commit this one has loaded. For after `update`:
    /// without it the others keep the index they have.
    pub async fn announce(&self) -> Result<()> {
        let commit = self.commit().await;
        // Before it is published, or this process would follow its own announcement
        self.mark_seen(&commit);

        let mut conn = self.redis_pool.get().await?;
        redis::pipe()
            .set(ANNOUNCED_INDEX, &commit)
            .publish(ANNOUNCED_INDEX, &commit)
            .exec_async(&mut *conn)
            .await?;
        tracing::info!("Announced index commit {commit} to the other lobby processes");

        Ok(())
    }

    /// Looks at what was announced and loads it if this process hasn't yet. Returns whether
    /// it loaded anything.
    ///
    /// If the announced commit isn't in the index repository, the newest one is loaded
    /// instead and announced in its place: the repository is what counts, and the process
    /// that announced the other one will follow.
    pub async fn follow_once(&self) -> Result<bool> {
        let Some(announced) = self.announced().await? else {
            return Ok(false);
        };
        if self.seen_announcement.lock().unwrap().as_deref() == Some(announced.as_str()) {
            return Ok(false);
        }
        if self.commit().await == announced {
            self.mark_seen(&announced);
            return Ok(false);
        }

        tracing::info!("Index commit {announced} was announced, loading it");
        let commit = self.load(Some(&announced)).await?;
        if commit == announced {
            self.mark_seen(&announced);
        } else {
            tracing::warn!(
                "The announced index commit {announced} isn't in the repository, loaded {commit}"
            );
            self.announce().await?;
        }

        Ok(true)
    }

    /// What a process does once it has started, having loaded the newest commit: if that
    /// isn't what was announced, either it is ahead and the others should come along, or it
    /// read the repository just before a commit that was announced since, and it catches up.
    pub async fn join_other_processes(&self) -> Result<()> {
        let commit = self.commit().await;
        match self.announced().await? {
            Some(announced) if announced == commit => self.mark_seen(&announced),
            Some(announced) if !is_ancestor(&self.source.index_path, &announced, &commit) => {
                self.follow_once().await?;
            }
            _ => self.announce().await?,
        }

        Ok(())
    }

    /// Keeps this process on the announced commit for as long as it runs: as soon as one is
    /// announced, and every `FOLLOW_INTERVAL` in case the message never arrived.
    pub fn start_following(&self) -> JoinHandle<()> {
        let manager = self.clone();

        tokio::spawn(async move {
            let mut announcements = Announcements::new(manager.redis_client.clone());
            loop {
                announcements.wait(FOLLOW_INTERVAL).await;
                if let Err(e) = manager.follow_once().await {
                    tracing::error!("Couldn't follow the announced index: {e:#}");
                }
            }
        })
    }

    /// What rooms on `base` see of the index. Fails for a base the index doesn't describe.
    pub async fn index_for(&self, base: &Version) -> Result<RwLockReadGuard<'_, Index>> {
        RwLockReadGuard::try_map(self.loaded.read().await, |loaded| loaded.index.get(base))
            .map_err(|_| anyhow!("The index doesn't describe Archipelago {base}"))
    }

    /// `archipelago_version` in the index: the base every room was on before rooms had one,
    /// and the one whose jobs keep the queues' original keys. Not necessarily on offer.
    pub async fn legacy_base(&self) -> Version {
        self.loaded.read().await.index.legacy_base.clone()
    }

    /// The bases a room can be made on and the options pages can be asked for, newest first:
    /// the ones this installation enables that the index describes. A room that is already
    /// on another base keeps working as far as `index_for` goes, but there is probably no
    /// worker for it.
    pub async fn offered_bases(&self) -> Vec<Version> {
        self.bases.offered(&self.loaded.read().await.index)
    }

    pub async fn offers(&self, base: &Version) -> bool {
        self.offered_bases().await.contains(base)
    }

    /// The base of a room that doesn't ask for one, and of everything that isn't about a
    /// room: the configured default, or the newest base on offer.
    pub async fn default_base(&self) -> Version {
        self.bases.default_base(&self.loaded.read().await.index)
    }

    /// `wanted` if it is on offer, the default base otherwise. For a base that was only ever
    /// a preference, such as a room template's.
    pub async fn base_or_default(&self, wanted: Option<&Version>) -> Version {
        let index = &self.loaded.read().await.index;
        match wanted {
            Some(wanted) if self.bases.offered(index).contains(wanted) => wanted.clone(),
            _ => self.bases.default_base(index),
        }
    }

    /// `index_for` the default base
    pub async fn default_index(&self) -> RwLockReadGuard<'_, Index> {
        RwLockReadGuard::map(self.loaded.read().await, |loaded| {
            let default = self.bases.default_base(&loaded.index);
            // The default base is one the index describes, or its own version
            loaded
                .index
                .get(&default)
                .unwrap_or_else(|| loaded.index.legacy())
        })
    }

    /// The whole index as it is now, to compare with after an update
    pub async fn snapshot(&self) -> IndexSet {
        self.loaded.read().await.index.clone()
    }

    /// Every release of every base in one index. For looking a release up when the base it is
    /// wanted for isn't known; no room sees the index this way.
    pub async fn all_releases(&self) -> Index {
        self.loaded.read().await.index.all_releases()
    }

    pub async fn get_apworld_from_game_name(
        &self,
        base: &Version,
        manifest: &Manifest,
        game_name: &str,
    ) -> Option<(String, Version)> {
        let index = self.index_for(base).await.ok()?;
        let (world, version) = manifest.resolve_from_game_name(game_name, &index).ok()?;
        let path = world.path.file_stem().unwrap().to_str().unwrap().to_owned();

        Some((path, version.clone()))
    }

    pub async fn download_apworlds(
        &self,
        base: &Version,
        manifest: &Manifest,
    ) -> Result<ZipFile<'_>> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let apworlds_path = &self.apworlds_path;
        let prefix = "custom_worlds";
        writer.add_directory(prefix, options)?;

        let index = self.index_for(base).await?;
        let mut buffer = Vec::new();
        let (worlds, resolve_errors) = manifest.resolve_with(&index);
        if !resolve_errors.is_empty() {
            bail!("Error while resolving manifest");
        }

        for (world_name, (world, version)) in &worlds {
            let origin = world.get_version(version).unwrap();

            if origin.is_supported() {
                continue;
            }

            let file_path = index.get_world_local_path(apworlds_path, world_name, version);
            writer.start_file(format!("{prefix}/{world_name}.apworld"), options)?;
            File::open(&file_path)
                .with_context(|| format!("Can't open {file_path:?}"))?
                .read_to_end(&mut buffer)?;
            writer.write_all(&buffer)?;
            buffer.clear();
        }

        let value = "attachment; filename=\"apworlds.zip\"";
        let content = writer.finish()?.into_inner();

        Ok(ZipFile {
            content,
            headers: Header::new(CONTENT_DISPOSITION.as_str(), value),
        })
    }
}

/// Wakes the follower when an index commit is announced. Losing the connection to Valkey
/// only costs the promptness: `wait` still returns after its timeout.
struct Announcements {
    client: redis::Client,
    subscription: Option<(
        redis::aio::MultiplexedConnection,
        tokio::sync::broadcast::Receiver<redis::PushInfo>,
    )>,
}

impl Announcements {
    fn new(client: redis::Client) -> Self {
        Self {
            client,
            subscription: None,
        }
    }

    async fn subscribe(&mut self) -> Result<()> {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let config = redis::AsyncConnectionConfig::new().set_push_sender(tx);
        let mut conn = self
            .client
            .get_multiplexed_async_connection_with_config(&config)
            .await?;
        conn.subscribe(ANNOUNCED_INDEX).await?;
        self.subscription = Some((conn, rx));

        Ok(())
    }

    /// Returns when something was published, or after `at_most`
    async fn wait(&mut self, at_most: Duration) {
        if self.subscription.is_none() {
            if let Err(e) = self.subscribe().await {
                tracing::warn!("Couldn't subscribe to index announcements: {e:#}");
            }
        }

        let Some((_, messages)) = &mut self.subscription else {
            tokio::time::sleep(at_most).await;
            return;
        };

        use tokio::sync::broadcast::error::RecvError;
        if let Ok(Err(RecvError::Closed)) = tokio::time::timeout(at_most, messages.recv()).await {
            // The connection is gone. The next call subscribes again.
            self.subscription = None;
        }
    }
}

/// Fetches the branch of the index repository into this process's checkout, and resets the
/// checkout to `wanted` if that commit exists there, to the head of the branch otherwise.
/// Returns the commit the checkout is at, and whether that is `wanted`.
fn fetch_and_reset(source: &IndexSource, wanted: Option<&str>) -> Result<(String, bool)> {
    let repo = Repository::init(&source.index_path)?;

    let mut remote = repo
        .find_remote("origin")
        .or_else(|_| repo.remote("origin", &source.repo_url))?;

    remote.fetch(&[&source.repo_branch], None, None)?;

    let wanted_commit = wanted
        .and_then(|wanted| Oid::from_str(wanted).ok())
        .and_then(|oid| repo.find_commit(oid).ok());
    let found_wanted = wanted_commit.is_some();
    let commit = match wanted_commit {
        Some(commit) => commit,
        None => repo.find_reference("FETCH_HEAD")?.peel_to_commit()?,
    };
    repo.reset(commit.as_object(), ResetType::Hard, None)?;

    Ok((commit.id().to_string(), found_wanted))
}

/// Whether `commit` has `ancestor` in its history, as far as the checkout at `index_path`
/// knows. A commit it doesn't have is nobody's ancestor.
fn is_ancestor(index_path: &Path, ancestor: &str, commit: &str) -> bool {
    let Ok(repo) = Repository::open(index_path) else {
        return false;
    };
    let (Ok(ancestor), Ok(commit)) = (Oid::from_str(ancestor), Oid::from_str(commit)) else {
        return false;
    };

    repo.graph_descendant_of(commit, ancestor).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{redis_pool, start_valkey, ValkeyInstance};
    use tempfile::TempDir;

    /// An index repository to commit to, a Valkey, and room for the checkouts of the lobby
    /// processes that read them
    struct Fixture {
        dir: TempDir,
        valkey: ValkeyInstance,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let origin = dir.path().join("origin");
            std::fs::create_dir_all(origin.join("index")).unwrap();
            Repository::init_opts(
                &origin,
                git2::RepositoryInitOptions::new().initial_head("main"),
            )
            .unwrap();
            std::fs::write(
                origin.join("index.toml"),
                concat!(
                    "archipelago_repo = \"https://example.invalid/Archipelago.git\"\n",
                    "archipelago_version = \"0.6.7\"\n",
                    "index_homepage = \"https://example.invalid\"\n",
                    "index_dir = \"index\"\n",
                ),
            )
            .unwrap();
            std::fs::write(origin.join("index.lock"), "[apworlds]\n").unwrap();

            let fixture = Self {
                dir,
                valkey: start_valkey(),
            };
            fixture.commit_world("first");

            fixture
        }

        fn origin(&self) -> PathBuf {
            self.dir.path().join("origin")
        }

        /// Adds a core world to the index repository, which needs no download. Returns the
        /// commit.
        fn commit_world(&self, apworld: &str) -> String {
            std::fs::write(
                self.origin().join("index").join(format!("{apworld}.toml")),
                format!(
                    "name = \"{apworld}\"\nhome = \"https://example.invalid\"\nsupported = true\n"
                ),
            )
            .unwrap();

            self.commit(apworld)
        }

        /// Commits whatever changed in the index repository. Returns the commit.
        fn commit(&self, message: &str) -> String {
            let repo = Repository::open(self.origin()).unwrap();
            let mut index = repo.index().unwrap();
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let author = git2::Signature::now("test", "test@example.invalid").unwrap();
            let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
            let parents: Vec<&git2::Commit> = parent.iter().collect();

            repo.commit(Some("HEAD"), &author, &author, message, &tree, &parents)
                .unwrap()
                .to_string()
        }

        /// A lobby process that has read the index and done nothing else yet
        fn read(&self, name: &str) -> IndexManager {
            self.configured(name, BaseConfig::default()).unwrap()
        }

        /// `read`, for an installation that sets `AP_BASES` or `AP_DEFAULT_BASE`
        fn configured(&self, name: &str, bases: BaseConfig) -> Result<IndexManager> {
            let source = IndexSource {
                repo_url: self.origin().to_string_lossy().to_string(),
                repo_branch: "main".to_string(),
                index_path: self.dir.path().join(name),
                apworlds_path: self.dir.path().join("apworlds"),
            };

            IndexManager::new(source, bases, redis_pool(&self.valkey), &self.valkey.url())
        }

        /// Has the index describe one more Archipelago version. Returns the commit.
        fn declare_base(&self, base: &str) -> String {
            let index_toml = self.origin().join("index.toml");
            let mut content = std::fs::read_to_string(&index_toml).unwrap();
            content.push_str(&format!("\n[bases.\"{base}\"]\n"));
            std::fs::write(index_toml, content).unwrap();

            self.commit(&format!("declare {base}"))
        }

        /// A lobby process as it is once it has started
        async fn process(&self, name: &str) -> IndexManager {
            let manager = self.read(name);
            manager.update().await.unwrap();
            manager.join_other_processes().await.unwrap();

            manager
        }

        async fn announced(&self) -> Option<String> {
            let mut conn = redis_pool(&self.valkey).get().await.unwrap();
            conn.get(ANNOUNCED_INDEX).await.unwrap()
        }
    }

    async fn worlds(manager: &IndexManager) -> Vec<String> {
        manager
            .default_index()
            .await
            .worlds
            .keys()
            .cloned()
            .collect()
    }

    fn bases(ap_bases: Option<&str>, ap_default_base: Option<&str>) -> BaseConfig {
        BaseConfig::parse(ap_bases, ap_default_base).unwrap()
    }

    fn versions(versions: &[&str]) -> Vec<Version> {
        versions.iter().map(|v| v.parse().unwrap()).collect()
    }

    #[test]
    fn test_base_config_is_read_from_its_two_values() {
        assert_eq!(bases(None, None), BaseConfig::default());
        // Set to nothing is not set
        assert_eq!(bases(Some(""), Some(" ")), BaseConfig::default());

        let config = bases(Some("0.6.8, 0.6.7,0.6.8"), Some("0.6.7"));
        assert_eq!(config.enabled, Some(versions(&["0.6.8", "0.6.7"])));
        assert_eq!(config.default, Some("0.6.7".parse().unwrap()));
    }

    #[test]
    fn test_base_config_refuses_what_it_cannot_mean() {
        let error = BaseConfig::parse(Some("0.6.8,latest"), None).unwrap_err();
        assert!(format!("{error:#}").contains("AP_BASES: \"latest\""));

        let error = BaseConfig::parse(Some("0.6.8,"), None).unwrap_err();
        assert!(format!("{error:#}").contains("AP_BASES: \"\""));

        let error = BaseConfig::parse(None, Some("newest")).unwrap_err();
        assert!(format!("{error:#}").contains("AP_DEFAULT_BASE: \"newest\""));

        let error = BaseConfig::parse(Some("0.6.8,0.6.7"), Some("0.6.9")).unwrap_err();
        assert!(format!("{error:#}").contains("isn't one of AP_BASES"));
    }

    #[rocket::async_test]
    async fn test_unconfigured_lobby_offers_the_index_version_only() {
        let fixture = Fixture::new();
        fixture.declare_base("0.6.8");
        let process = fixture.process("process").await;

        assert_eq!(process.offered_bases().await, versions(&["0.6.7"]));
        assert_eq!(process.default_base().await, Version::new(0, 6, 7));
        assert!(!process.offers(&Version::new(0, 6, 8)).await);
        // What it doesn't offer is still there for a room that is on it
        assert!(process.index_for(&Version::new(0, 6, 8)).await.is_ok());
    }

    #[rocket::async_test]
    async fn test_enabled_base_is_offered_once_the_index_describes_it() {
        let fixture = Fixture::new();
        let process = fixture
            .configured("process", bases(Some("0.6.8,0.6.7"), None))
            .unwrap();

        // Enabled ahead of the index
        assert_eq!(process.offered_bases().await, versions(&["0.6.7"]));
        assert_eq!(process.default_base().await, Version::new(0, 6, 7));

        fixture.declare_base("0.6.8");
        process.update().await.unwrap();

        // Newest first, and the newest is the default
        assert_eq!(process.offered_bases().await, versions(&["0.6.8", "0.6.7"]));
        assert_eq!(process.default_base().await, Version::new(0, 6, 8));
        assert_eq!(
            process.default_index().await.archipelago_version,
            Version::new(0, 6, 8)
        );
    }

    #[rocket::async_test]
    async fn test_configured_default_base_wins_over_the_newest() {
        let fixture = Fixture::new();
        fixture.declare_base("0.6.8");
        let process = fixture
            .configured("process", bases(Some("0.6.8,0.6.7"), Some("0.6.7")))
            .unwrap();

        assert_eq!(process.offered_bases().await, versions(&["0.6.8", "0.6.7"]));
        assert_eq!(process.default_base().await, Version::new(0, 6, 7));
        assert_eq!(
            process.default_index().await.archipelago_version,
            Version::new(0, 6, 7)
        );
    }

    #[rocket::async_test]
    async fn test_index_version_can_be_left_out() {
        let fixture = Fixture::new();
        let process = fixture
            .configured("process", bases(Some("0.6.8"), None))
            .unwrap();

        // Nothing it enables exists yet. What has no base falls back on the index's version.
        assert!(process.offered_bases().await.is_empty());
        assert_eq!(process.default_base().await, Version::new(0, 6, 7));

        fixture.declare_base("0.6.8");
        process.update().await.unwrap();

        assert_eq!(process.offered_bases().await, versions(&["0.6.8"]));
        assert_eq!(process.default_base().await, Version::new(0, 6, 8));
        assert!(!process.offers(&Version::new(0, 6, 7)).await);
        // The queues' original keys stay with the index's version all the same
        assert_eq!(process.legacy_base().await, Version::new(0, 6, 7));
    }

    #[rocket::async_test]
    async fn test_default_base_that_is_not_offered_stops_the_lobby() {
        let fixture = Fixture::new();
        fixture.declare_base("0.6.8");

        let error = fixture
            .configured("process", bases(None, Some("0.6.8")))
            .err()
            .expect("Only 0.6.7 is offered without AP_BASES");
        assert!(format!("{error:#}").contains("without AP_BASES"));

        // Naming the only base there is changes nothing
        assert!(fixture
            .configured("other", bases(None, Some("0.6.7")))
            .is_ok());
    }

    #[rocket::async_test]
    async fn test_preferred_base_is_kept_only_if_it_is_offered() {
        let fixture = Fixture::new();
        fixture.declare_base("0.6.8");
        fixture.declare_base("0.6.9");
        let process = fixture
            .configured("process", bases(Some("0.6.8,0.6.7"), None))
            .unwrap();
        let (old, new, newest) = (
            Version::new(0, 6, 7),
            Version::new(0, 6, 8),
            Version::new(0, 6, 9),
        );

        assert_eq!(process.base_or_default(Some(&old)).await, old);
        assert_eq!(process.base_or_default(Some(&new)).await, new);
        assert_eq!(process.base_or_default(None).await, new);
        // The index describes it, this lobby doesn't offer it
        assert_eq!(process.base_or_default(Some(&newest)).await, new);
    }

    #[rocket::async_test]
    async fn test_refresh_reaches_a_process_that_follows() {
        let fixture = Fixture::new();
        let refreshed = fixture.process("refreshed").await;
        let other = fixture.process("other").await;
        let following = other.start_following();

        let commit = fixture.commit_world("second");
        assert_eq!(refreshed.update().await.unwrap(), commit);
        assert_eq!(worlds(&other).await, ["first"]);

        refreshed.announce().await.unwrap();

        // Told by the message, long before it would look by itself
        let told_within = Duration::from_secs(10);
        assert!(told_within < FOLLOW_INTERVAL);
        let deadline = tokio::time::Instant::now() + told_within;
        while other.commit().await != commit {
            assert!(
                tokio::time::Instant::now() < deadline,
                "The other process never loaded the announced commit"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(worlds(&other).await, ["first", "second"]);

        following.abort();
    }

    #[rocket::async_test]
    async fn test_missed_announcement_is_found_at_the_next_look() {
        let fixture = Fixture::new();
        let refreshed = fixture.process("refreshed").await;
        // Never subscribed, as a process whose connection dropped at the wrong time
        let other = fixture.process("other").await;

        let commit = fixture.commit_world("second");
        refreshed.update().await.unwrap();
        refreshed.announce().await.unwrap();

        assert!(other.follow_once().await.unwrap());
        assert_eq!(other.commit().await, commit);
        assert_eq!(worlds(&other).await, ["first", "second"]);

        // And nothing is loaded again for the same announcement, by either of them
        assert!(!other.follow_once().await.unwrap());
        assert!(!refreshed.follow_once().await.unwrap());
    }

    #[rocket::async_test]
    async fn test_followers_load_the_announced_commit_not_the_newest() {
        let fixture = Fixture::new();
        let refreshed = fixture.process("refreshed").await;
        let other = fixture.process("other").await;

        let announced = fixture.commit_world("second");
        refreshed.update().await.unwrap();
        refreshed.announce().await.unwrap();
        // The repository moves on before the other process has looked
        fixture.commit_world("third");

        assert!(other.follow_once().await.unwrap());
        assert_eq!(other.commit().await, announced);
        assert_eq!(worlds(&other).await, worlds(&refreshed).await);
    }

    #[rocket::async_test]
    async fn test_first_process_announces_what_it_loaded() {
        let fixture = Fixture::new();
        assert_eq!(fixture.announced().await, None);

        let first = fixture.process("first").await;

        assert_eq!(fixture.announced().await, Some(first.commit().await));
    }

    #[rocket::async_test]
    async fn test_process_that_starts_on_a_newer_commit_takes_the_others_along() {
        let fixture = Fixture::new();
        let running = fixture.process("running").await;

        // Nobody refreshed after this commit. A process that starts reads it anyway.
        let commit = fixture.commit_world("second");
        let started = fixture.process("started").await;
        assert_eq!(started.commit().await, commit);
        assert_eq!(fixture.announced().await, Some(commit.clone()));

        assert!(running.follow_once().await.unwrap());
        assert_eq!(running.commit().await, commit);
    }

    #[rocket::async_test]
    async fn test_process_that_read_just_before_a_refresh_catches_up() {
        let fixture = Fixture::new();
        let running = fixture.process("running").await;
        let starting = fixture.read("starting");

        let commit = fixture.commit_world("second");
        running.update().await.unwrap();
        running.announce().await.unwrap();

        // It holds an older commit than the announced one, and must not announce its own
        starting.join_other_processes().await.unwrap();

        assert_eq!(starting.commit().await, commit);
        assert_eq!(fixture.announced().await, Some(commit));
        assert!(!running.follow_once().await.unwrap());
    }

    #[rocket::async_test]
    async fn test_announced_commit_that_does_not_exist_is_replaced() {
        let fixture = Fixture::new();
        let process = fixture.process("process").await;
        let newest = fixture.commit_world("second");

        let mut conn = redis_pool(&fixture.valkey).get().await.unwrap();
        conn.set::<_, _, ()>(ANNOUNCED_INDEX, "0123456789012345678901234567890123456789")
            .await
            .unwrap();

        assert!(process.follow_once().await.unwrap());
        assert_eq!(process.commit().await, newest);
        assert_eq!(fixture.announced().await, Some(newest));
        assert!(!process.follow_once().await.unwrap());
    }
}
