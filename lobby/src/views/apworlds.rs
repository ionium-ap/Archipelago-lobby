use crate::db;
use crate::db::OpenState;
use crate::db::RoomFilter;
use crate::jobs::YamlValidationQueue;
use crate::session::Session;
use crate::yaml::revalidate_yamls_if_necessary;
use anyhow::Context as _;
use apwm::Index;
use apwm::Manifest;
use apwm::World;
use askama::Template;
use askama_web::WebTemplate;
use http::header::CONTENT_DISPOSITION;
use rocket::fs::NamedFile;
use rocket::http::Header;
use rocket::routes;
use rocket::State;
use semver::Version;
use std::collections::BTreeMap;

use crate::error::Result;
use crate::index_manager::IndexManager;
use crate::session::{AdminSession, LoggedInSession};
use crate::utils::{RenamedFile, ZipFile};
use crate::views::filters;
use crate::{Context, LobbyConfig, TplContext};

#[derive(Template, WebTemplate)]
#[template(path = "apworld/main.html")]
struct WorldsListTpl<'a> {
    base: TplContext<'a>,
    index: Index,
    apworlds: Vec<(String, (World, Version))>,
}

#[rocket::get("/worlds")]
#[tracing::instrument(skip_all)]
async fn list_worlds<'a>(
    index_manager: &'a State<IndexManager>,
    session: Session,
    ctx: &State<Context>,
    lobby_config: &State<LobbyConfig>,
) -> Result<WorldsListTpl<'a>> {
    let index = index_manager.default_index().await.clone();
    let manifest = Manifest::from_index_with_default_versions(&index)?;
    let (apworlds, _) = manifest.resolve_with(&index);
    let mut apworlds = Vec::from_iter(apworlds);
    apworlds.sort_by_key(|(_, (world, _))| world.display_name.to_lowercase());

    Ok(WorldsListTpl {
        base: TplContext::from_session(
            "apworlds",
            session,
            ctx,
            lobby_config,
            Some("Apworlds".to_string()),
        )
        .await,
        index,
        apworlds,
    })
}

#[rocket::get("/worlds/download_all")]
#[tracing::instrument(skip_all)]
async fn download_all(
    index_manager: &State<IndexManager>,
    _session: LoggedInSession,
) -> Result<ZipFile<'_>> {
    let base = index_manager.default_base().await;
    let manifest = {
        let index = index_manager.index_for(&base).await?;
        Manifest::from_index_with_default_versions(&index)?
    };
    Ok(index_manager.download_apworlds(&base, &manifest).await?)
}

#[rocket::get("/worlds/download/<world_name>/<version>")]
#[tracing::instrument(skip(index_manager, _session))]
async fn download_world<'a>(
    index_manager: &State<IndexManager>,
    version: &str,
    world_name: &str,
    _session: LoggedInSession,
) -> Result<RenamedFile<'a>> {
    // A release is one file whichever Archipelago version it is for, so any release that any
    // of them has can be downloaded here.
    let index = index_manager.all_releases().await;

    let world = index
        .worlds
        .get(world_name)
        .context("This APworld doesn't seem to exist")?;

    let version = semver::Version::parse(version).context("The passed version isn't valid")?;

    let _origin = world
        .get_version(&version)
        .context("The specified version doesn't exist for this apworld")?;

    let apworld_path = index_manager
        .apworlds_path
        .join(format!("{world_name}-{version}.apworld"));

    if !apworld_path.exists() {
        return Err(anyhow::anyhow!(
            "This apworld seems to be in the host's index but not in their apworld folder."
        )
        .into());
    }

    let value = format!("attachment; filename=\"{world_name}.apworld\"");
    return Ok(RenamedFile {
        inner: NamedFile::open(&apworld_path).await?,
        headers: Header::new(CONTENT_DISPOSITION.as_str(), value),
    });
}

#[rocket::get("/worlds/refresh")]
#[tracing::instrument(skip_all)]
async fn refresh_worlds(
    index_manager: &State<IndexManager>,
    yaml_validation_queue: &State<YamlValidationQueue>,
    ctx: &State<Context>,
    _session: AdminSession,
) -> Result<()> {
    let old_index = index_manager.snapshot().await;
    index_manager.update().await?;
    // The other lobby processes load the same commit when they hear of it. The rooms are
    // looked at once, here: what is found goes to the database and the queues, which they
    // all share. A failure to announce is only reported once that is done.
    let announced = index_manager.announce().await;

    let mut conn = ctx.db_pool.get().await?;

    let (open_rooms, _) = db::list_rooms(
        RoomFilter::default().with_open_state(OpenState::Open),
        None,
        &mut conn,
    )
    .await?;

    for room in &open_rooms {
        if index_manager.index_for(&room.ap_version).await.is_err() {
            tracing::warn!(
                room_id = %room.id,
                "The index no longer describes Archipelago {}, leaving the room's YAMLs as they are",
                room.ap_version
            );
            continue;
        }

        // What changed for a room is what changed in the view of its Archipelago version. A
        // version the index didn't describe before had nothing, so everything is new to it.
        let old_resolved = match old_index.get(&room.ap_version) {
            Some(old_view) => room.settings.manifest.resolve_with(old_view).0,
            None => BTreeMap::new(),
        };
        revalidate_yamls_if_necessary(
            room,
            &old_resolved,
            index_manager,
            yaml_validation_queue,
            &mut conn,
        )
        .await?;
    }

    announced
        .context("The index was refreshed here, but the other lobby processes weren't told")?;

    Ok(())
}

pub fn routes() -> Vec<rocket::Route> {
    routes![list_worlds, download_all, download_world, refresh_worlds]
}
