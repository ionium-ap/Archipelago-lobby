use crate::base_switch::{switch_report, SwitchReport};
use crate::db::{self, ApVersion, Room, RoomId, RoomTemplateId};
use crate::error::{Error, RedirectTo, Result, WithContext};
use crate::index_manager::IndexManager;
use crate::jobs::YamlValidationQueue;
use crate::session::LoggedInSession;
use crate::yaml::{games_of_yaml, queue_yaml_validation, revalidate_yamls_if_necessary};
use askama::Template;
use askama_web::WebTemplate;
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::AsyncConnection;
use rocket::either::Either;
use rocket::form::Form;
use rocket::response::Redirect;
use rocket::State;
use rocket::{get, post};
use semver::Version;

use crate::{Context, LobbyConfig, TplContext};

use crate::views::options_gen::requested_base;
use crate::views::room_settings::{
    validate_room_form, BaseSelect, CreateRoomForm, RoomSettingsBuilder, RoomSettingsType,
};
use crate::views::utils::base_options;

#[derive(Template, WebTemplate)]
#[template(path = "room/edit.html")]
pub struct EditRoom<'a> {
    base: TplContext<'a>,
    room: Option<Room>,
    room_settings_form: RoomSettingsBuilder<'a>,
}

/// The Archipelago version of a room that is being made: the one the form asks for with
/// `base`, which has to be on offer, or else the template's if it is still offered, or else
/// the default one.
async fn new_room_base(
    index_manager: &IndexManager,
    base: Option<&str>,
    template_version: Option<&Version>,
) -> anyhow::Result<Version> {
    match base {
        Some(base) => requested_base(index_manager, Some(base)).await,
        None => Ok(index_manager.base_or_default(template_version).await),
    }
}

#[get("/create-room?<from_template>&<base>")]
#[tracing::instrument(skip_all)]
pub async fn create_room<'a>(
    from_template: Option<RoomTemplateId>,
    base: Option<&str>,
    session: LoggedInSession,
    index_manager: &State<IndexManager>,
    ctx: &State<Context>,
    lobby_config: &State<LobbyConfig>,
    redirect_to: &RedirectTo,
) -> Result<EditRoom<'a>> {
    if lobby_config.admin_rooms_only && !session.is_admin() {
        return Err(anyhow::anyhow!("Room creation is restricted to admins only").into());
    }
    // Without a template or a version the form always works
    redirect_to.set("/create-room");
    let current_user_id = session.user_id();
    let requested = base;
    let base = TplContext::from_session(
        "create-room",
        session.0,
        ctx,
        lobby_config,
        Some("Create New Room".to_string()),
    )
    .await;
    let template = match from_template {
        Some(template_id) => {
            let mut conn = ctx.db_pool.get().await?;
            let template = db::get_room_template_by_id(template_id, &mut conn)
                .await
                .context("Couldn't get the specified template")?;
            (template.global || template.settings.author_id == current_user_id).then_some(template)
        }
        None => None,
    };

    // The form shows the worlds of one Archipelago version. Choosing another one loads it
    // again with `base`, and what it is posted to carries the same `base`.
    let template_version = template.as_ref().and_then(|tpl| tpl.ap_version.as_deref());
    let ap_version = new_room_base(index_manager, requested, template_version).await?;
    let index = index_manager.index_for(&ap_version).await?;

    let offered = index_manager.offered_bases().await;
    let base_select = (offered.len() > 1).then(|| BaseSelect {
        options: base_options(&offered, Some(&ap_version)),
        switch_url: None,
        locked: None,
    });

    let form_builder = match template {
        Some(template) => {
            RoomSettingsBuilder::room_from_template(base.clone(), index.clone(), template)?
        }
        None => RoomSettingsBuilder::new(base.clone(), &index, RoomSettingsType::Room)?,
    }
    .with_base_select(base_select);

    Ok(EditRoom {
        room: None,
        room_settings_form: form_builder,
        base,
    })
}

#[post("/create-room?<from_template>&<base>", data = "<room_form>")]
#[tracing::instrument(skip_all)]
pub async fn create_room_submit<'a>(
    from_template: Option<RoomTemplateId>,
    base: Option<&str>,
    redirect_to: &RedirectTo,
    ctx: &State<Context>,
    index_manager: &State<IndexManager>,
    mut room_form: Form<CreateRoomForm<'a>>,
    session: LoggedInSession,
    lobby_config: &State<LobbyConfig>,
) -> Result<Redirect> {
    if lobby_config.admin_rooms_only && !session.is_admin() {
        return Err(anyhow::anyhow!("Room creation is restricted to admins only").into());
    }
    redirect_to.set("/create-room");

    validate_room_form(&mut room_form.room)?;

    let mut conn = ctx.db_pool.get().await?;
    let mut template_version = None;
    if let Some(template_id) = from_template {
        let tpl = db::get_room_template_by_id(template_id, &mut conn)
            .await
            .context("The given template couldn't be found")?;
        if !tpl.global && tpl.settings.author_id != session.user_id() {
            Err(anyhow::anyhow!("The given template couldn't be found"))?
        }
        template_version = tpl.ap_version;
    }

    // The same choice of Archipelago version as the form this answers was built with
    let ap_version = new_room_base(index_manager, base, template_version.as_deref()).await?;
    let new_room = {
        let index = index_manager.index_for(&ap_version).await?;
        room_form.room.to_new_room(
            RoomId::new_v4(),
            &index,
            ap_version.into(),
            Some(session.user_id()),
            Some(from_template),
        )?
    };

    let new_room = db::create_room(&new_room, &mut conn).await?;

    Ok(Redirect::to(format!("/room/{}", new_room.id)))
}

#[get("/edit-room/<room_id>")]
#[tracing::instrument(skip(ctx, session, index_manager))]
pub async fn edit_room<'a>(
    ctx: &State<Context>,
    room_id: RoomId,
    session: LoggedInSession,
    index_manager: &State<IndexManager>,
    lobby_config: &State<LobbyConfig>,
) -> Result<EditRoom<'a>> {
    let mut conn = ctx.db_pool.get().await?;
    let room = db::get_room(room_id, &mut conn).await?;
    let is_my_room = session.0.is_admin || session.0.user_id == Some(room.settings.author_id);

    if !is_my_room {
        return Err(anyhow::anyhow!("You're not allowed to edit this room").into());
    }

    let index = index_manager.index_for(&room.ap_version).await?;
    let base = TplContext::from_session(
        "room",
        session.0,
        ctx,
        lobby_config,
        Some(format!("{} - Edit Room", room.settings.name)),
    )
    .await;

    // The room can move to another Archipelago version if there is one, and until it has been
    // generated.
    let offered = index_manager.offered_bases().await;
    let has_other_base = offered.iter().any(|base| base != &*room.ap_version);
    let base_select = if has_other_base {
        let generated = db::get_generation_for_room(room.id, &mut conn)
            .await?
            .is_some();
        Some(BaseSelect {
            options: base_options(&offered, Some(&room.ap_version)),
            switch_url: Some(format!("/edit-room/{}/base", room.id)),
            locked: generated.then(|| {
                "A generation exists for this room, and it was made with this version.".to_string()
            }),
        })
    } else {
        None
    };

    Ok(EditRoom {
        room_settings_form: RoomSettingsBuilder::new_with_room(
            base.clone(),
            index.clone(),
            room.clone(),
        )
        .with_base_select(base_select),
        room: Some(room),
        base,
    })
}

#[derive(rocket::FromForm)]
pub struct SwitchBaseForm<'a> {
    /// The Archipelago version to move the room to
    to: &'a str,
    /// Set by the page that says what the move does
    #[field(default = false)]
    confirmed: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "room/switch_base.html")]
pub struct SwitchBaseTpl<'a> {
    base: TplContext<'a>,
    room: Room,
    target: Version,
    yaml_count: usize,
    report: SwitchReport,
}

/// Moves a room to another Archipelago version. Its manifest stays as it is and is read
/// against the worlds of the new version from then on, and every YAML is validated again.
///
/// A room with YAMLs in it only moves once its owner has seen what that does to them: the
/// first request answers with a page that says so, and that page asks again with `confirmed`.
#[post("/edit-room/<room_id>/base", data = "<form>")]
#[tracing::instrument(skip(
    redirect_to,
    form,
    index_manager,
    ctx,
    session,
    yaml_validation_queue,
    lobby_config
))]
pub async fn switch_base<'a>(
    redirect_to: &RedirectTo,
    room_id: RoomId,
    form: Form<SwitchBaseForm<'_>>,
    ctx: &State<Context>,
    index_manager: &State<IndexManager>,
    yaml_validation_queue: &State<YamlValidationQueue>,
    session: LoggedInSession,
    lobby_config: &State<LobbyConfig>,
) -> Result<Either<Redirect, SwitchBaseTpl<'a>>> {
    redirect_to.set(&format!("/edit-room/{room_id}"));

    let mut conn = ctx.db_pool.get().await?;
    let room = db::get_room(room_id, &mut conn).await?;
    let is_my_room = session.0.is_admin || session.0.user_id == Some(room.settings.author_id);
    if !is_my_room {
        return Err(anyhow::anyhow!("You're not allowed to edit this room").into());
    }

    let target = requested_base(index_manager, Some(form.to)).await?;
    if target == *room.ap_version {
        return Ok(Either::Left(Redirect::to(format!("/edit-room/{room_id}"))));
    }
    if db::get_generation_for_room(room.id, &mut conn)
        .await?
        .is_some()
    {
        Err(anyhow::anyhow!(
            "This room has a generation, made with Archipelago {}. It can't move to another version.",
            room.ap_version
        ))?
    }

    let yamls = db::get_yamls_for_room(room.id, &mut conn).await?;
    if !yamls.is_empty() && !form.confirmed {
        let report = {
            let current = index_manager.index_for(&room.ap_version).await?;
            let target_index = index_manager.index_for(&target).await?;
            let games: Vec<String> = yamls.iter().flat_map(games_of_yaml).collect();
            switch_report(
                &room.settings.manifest,
                &current,
                &target_index,
                games.iter().map(String::as_str),
            )
        };

        return Ok(Either::Right(SwitchBaseTpl {
            base: TplContext::from_session(
                "room",
                session.0,
                ctx,
                lobby_config,
                Some(format!("{} - Archipelago version", room.settings.name)),
            )
            .await,
            yaml_count: yamls.len(),
            room,
            target,
            report,
        }));
    }

    // Whatever a YAML was validated with says nothing about it on the new version, so they
    // all go through validation again. A result that is still on its way from the old one is
    // dropped when it arrives, see the validation callback.
    let target = ApVersion::from(target);
    conn.transaction::<(), Error, _>(|conn| {
        async move {
            db::update_room_ap_version(room.id, &target, conn).await?;
            let room = db::get_room(room.id, conn).await?;
            if room.settings.yaml_validation {
                for yaml in &yamls {
                    queue_yaml_validation(yaml, &room, index_manager, yaml_validation_queue, conn)
                        .await?;
                }
            }

            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Either::Left(Redirect::to(format!("/room/{room_id}"))))
}

#[get("/edit-room/<room_id>/delete")]
#[tracing::instrument(skip(ctx, session))]
pub async fn delete_room(
    ctx: &State<Context>,
    room_id: RoomId,
    session: LoggedInSession,
) -> Result<Redirect> {
    let mut conn = ctx.db_pool.get().await?;
    let room = db::get_room(room_id, &mut conn).await?;
    let is_my_room = session.0.is_admin || session.0.user_id == Some(room.settings.author_id);

    if !is_my_room {
        return Err(anyhow::anyhow!("You're not allowed to delete this room").into());
    }

    db::delete_room(room_id, &mut conn).await?;

    Ok(Redirect::to("/"))
}

#[post("/edit-room/<room_id>", data = "<room_form>")]
#[tracing::instrument(skip(
    redirect_to,
    room_form,
    index_manager,
    ctx,
    session,
    yaml_validation_queue
))]
pub async fn edit_room_submit<'a>(
    redirect_to: &RedirectTo,
    room_id: RoomId,
    mut room_form: Form<CreateRoomForm<'a>>,
    ctx: &State<Context>,
    index_manager: &State<IndexManager>,
    yaml_validation_queue: &State<YamlValidationQueue>,
    session: LoggedInSession,
) -> Result<Redirect> {
    redirect_to.set(&format!("/edit-room/{room_id}"));

    let mut conn = ctx.db_pool.get().await?;
    let room = db::get_room(room_id, &mut conn).await?;
    let is_my_room = session.0.is_admin || session.0.user_id == Some(room.settings.author_id);
    if !is_my_room {
        return Err(anyhow::anyhow!("You're not allowed to edit this room").into());
    }

    validate_room_form(&mut room_form.room)?;

    let (old_resolved, new_room) = {
        let index = index_manager.index_for(&room.ap_version).await?;
        let old_resolved = room.settings.manifest.resolve_with(&index).0;
        // author_id and from_template_id are None to skip updating those fields.
        // The room stays on the Archipelago version it is on.
        let new_room =
            room_form
                .room
                .to_new_room(room_id, &index, room.ap_version.clone(), None, None)?;
        (old_resolved, new_room)
    };

    let room = db::update_room(&new_room, &mut conn).await?;
    revalidate_yamls_if_necessary(
        &room,
        &old_resolved,
        index_manager,
        yaml_validation_queue,
        &mut conn,
    )
    .await?;

    Ok(Redirect::to(format!("/room/{room_id}")))
}

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        create_room,
        edit_room,
        delete_room,
        create_room_submit,
        edit_room_submit,
        switch_base
    ]
}
