use crate::db::Room;
use crate::error::Result;
use crate::{
    db::{self, ApVersion, NewRoomTemplate, RoomTemplate, RoomTemplateId},
    error::RedirectTo,
    index_manager::IndexManager,
    session::LoggedInSession,
};
use anyhow::anyhow;
use askama::Template;
use askama_web::WebTemplate;
use rocket::FromForm;
use rocket::{form::Form, get, post, response::Redirect, State};
use semver::Version;
use std::str::FromStr;

use crate::{Context, LobbyConfig, TplContext};

use super::manifest_editor::manifest_from_form;
use super::options_gen::requested_base;
use super::room_settings::{
    parse_date, validate_room_form, BaseSelect, RoomSettingsBuilder, RoomSettingsForm,
    RoomSettingsType,
};
use super::utils::{base_options, BaseOption};

#[derive(Debug, FromForm)]
pub struct CreateTplForm<'a> {
    room: RoomSettingsForm<'a>,
    tpl_name: &'a str,
    #[field(default = false)]
    tpl_global: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "room_templates/list.html")]
pub struct ListRoomTemplatesTpl<'a> {
    base: TplContext<'a>,
    room_templates: Vec<RoomTemplate>,
}

#[derive(Template, WebTemplate)]
#[template(path = "room_templates/edit.html")]
pub struct EditRoomTemplateTpl<'a> {
    base: TplContext<'a>,
    tpl: Option<RoomTemplate>,
    tpl_settings_form: RoomSettingsBuilder<'a>,
}

#[derive(Template, WebTemplate)]
#[template(path = "room_templates/rooms.html")]
pub struct AssociatedRoomsTpl<'a> {
    base: TplContext<'a>,
    tpl: RoomTemplate,
    rooms: Vec<Room>,
    current_page: u64,
    max_pages: u64,
}

#[get("/room-templates")]
#[tracing::instrument(skip_all)]
async fn list_templates<'a>(
    ctx: &State<Context>,
    session: LoggedInSession,
    lobby_config: &State<LobbyConfig>,
) -> Result<ListRoomTemplatesTpl<'a>> {
    let mut conn = ctx.db_pool.get().await?;
    let room_templates = db::get_room_templates_for_author(session.user_id(), &mut conn).await?;

    Ok(ListRoomTemplatesTpl {
        base: TplContext::from_session(
            "room-templates",
            session.0,
            ctx,
            lobby_config,
            Some("Room Templates".to_string()),
        )
        .await,
        room_templates,
    })
}

/// The Archipelago version a template names, going by the `base` its form asks for. Nothing
/// asked leaves it as it is, an empty `base` is a template that names no version, and a
/// version has to be on offer unless it is the one the template names already.
async fn template_base(
    index_manager: &IndexManager,
    base: Option<&str>,
    current: Option<&Version>,
) -> anyhow::Result<Option<Version>> {
    match base {
        None => Ok(current.cloned()),
        Some("") => Ok(None),
        Some(base) => match current {
            Some(current) if base == current.to_string() => Ok(Some(current.clone())),
            _ => Ok(Some(requested_base(index_manager, Some(base)).await?)),
        },
    }
}

/// The selector of a template form, when there is something to choose
async fn template_base_select(
    index_manager: &IndexManager,
    named: Option<&Version>,
) -> Option<BaseSelect> {
    let offered = index_manager.offered_bases().await;
    if offered.len() < 2 && named.is_none() {
        return None;
    }

    let mut options = vec![BaseOption {
        value: String::new(),
        label: "The default one when a room is made".to_string(),
        selected: named.is_none(),
    }];
    options.extend(base_options(&offered, named));

    Some(BaseSelect {
        options,
        switch_url: None,
        locked: None,
    })
}

#[get("/room-templates/create?<base>")]
#[tracing::instrument(skip_all)]
async fn create_template<'a>(
    base: Option<&str>,
    index_manager: &State<IndexManager>,
    session: LoggedInSession,
    ctx: &State<Context>,
    lobby_config: &State<LobbyConfig>,
    redirect_to: &RedirectTo,
) -> Result<EditRoomTemplateTpl<'a>> {
    redirect_to.set("/room-templates/create");
    // The form shows the worlds of the version the template names, or of the default one.
    // Choosing another loads it again with `base`, and it is posted with the same `base`.
    let named = template_base(index_manager, base, None).await?;
    let ap_version = index_manager.base_or_default(named.as_ref()).await;
    let index = index_manager.index_for(&ap_version).await?;
    let base_select = template_base_select(index_manager, named.as_ref()).await;

    let base = TplContext::from_session(
        "room-templates",
        session.0,
        ctx,
        lobby_config,
        Some("Create Room Template".to_string()),
    )
    .await;
    Ok(EditRoomTemplateTpl {
        tpl: None,
        tpl_settings_form: RoomSettingsBuilder::new(
            base.clone(),
            &index,
            RoomSettingsType::Template,
        )?
        .with_base_select(base_select),
        base,
    })
}

#[post("/room-templates/create?<base>", data = "<tpl_form>")]
#[tracing::instrument(skip_all)]
async fn create_tpl_submit<'a>(
    base: Option<&str>,
    redirect_to: &RedirectTo,
    ctx: &State<Context>,
    index_manager: &State<IndexManager>,
    mut tpl_form: Form<CreateTplForm<'a>>,
    session: LoggedInSession,
) -> Result<Redirect> {
    redirect_to.set("/room-templates/create");

    validate_tpl_form(&mut tpl_form)?;
    let named = template_base(index_manager, base, None).await?;
    let room_manifest = {
        let ap_version = index_manager.base_or_default(named.as_ref()).await;
        let index = index_manager.index_for(&ap_version).await?;
        manifest_from_form(&tpl_form.room.me, &index)
    }?;

    let author_id = session.user_id();
    let close_date = parse_date(tpl_form.room.close_date, tpl_form.room.tz_offset)?;
    let new_tpl = NewRoomTemplate {
        id: RoomTemplateId::new_v4(),
        name: tpl_form.room.room_name.trim(),
        close_date: close_date.naive_utc(),
        description: tpl_form.room.room_description.trim(),
        room_url: "",
        author_id: Some(author_id),
        yaml_validation: tpl_form.room.yaml_validation,
        allow_unsupported: tpl_form.room.allow_unsupported,
        yaml_limit_per_user: tpl_form
            .room
            .yaml_limit_per_user
            .then_some(tpl_form.room.yaml_limit_per_user_nb),
        yaml_limit_bypass_list: tpl_form
            .room
            .yaml_limit_bypass_list
            .split(',')
            .filter_map(|id| i64::from_str(id).ok())
            .collect(),
        manifest: db::Json(room_manifest),
        show_apworlds: tpl_form.room.show_apworlds,
        tpl_name: tpl_form.tpl_name,
        global: tpl_form.tpl_global && session.0.is_admin,
        meta_file: tpl_form.room.meta_file.clone(),
        is_bundle_room: tpl_form.room.is_bundle_room,
        locked: tpl_form.room.locked,
        ap_version: Some(named.map(ApVersion::from)),
    };

    let mut conn = ctx.db_pool.get().await?;
    db::create_room_template(&new_tpl, &mut conn).await?;

    Ok(Redirect::to("/room-templates"))
}

#[get("/room-templates/<tpl_id>?<base>")]
#[tracing::instrument(skip_all)]
async fn edit_template<'a>(
    tpl_id: RoomTemplateId,
    base: Option<&str>,
    index_manager: &State<IndexManager>,
    ctx: &State<Context>,
    session: LoggedInSession,
    lobby_config: &State<LobbyConfig>,
    redirect_to: &RedirectTo,
) -> Result<EditRoomTemplateTpl<'a>> {
    redirect_to.set(&format!("/room-templates/{tpl_id}"));
    let mut conn = ctx.db_pool.get().await?;
    let template = db::get_room_template_by_id(tpl_id, &mut conn).await?;
    let is_my_template = template.settings.author_id == session.user_id();
    if !is_my_template && !template.global {
        Err(anyhow!("You are not allowed to edit this template"))?;
    }

    // A template's manifest is about the worlds of the Archipelago version its rooms get.
    // `base` is a version chosen in the form and not saved yet.
    let named = template_base(index_manager, base, template.ap_version.as_deref()).await?;
    let ap_version = index_manager.base_or_default(named.as_ref()).await;
    let index = index_manager.index_for(&ap_version).await?;
    let base_select = template_base_select(index_manager, named.as_ref()).await;
    // Saving says which version, even when that is none
    let action_query = format!(
        "?base={}",
        named
            .as_ref()
            .map(|base| base.to_string().replace('+', "%2B"))
            .unwrap_or_default()
    );

    let base = TplContext::from_session(
        "template",
        session.0,
        ctx,
        lobby_config,
        Some(format!("Room Template - {}", template.tpl_name)),
    )
    .await;
    Ok(EditRoomTemplateTpl {
        tpl: Some(template.clone()),
        tpl_settings_form: RoomSettingsBuilder::new_with_template(
            base.clone(),
            index.clone(),
            template,
        )
        .read_only(!is_my_template)
        .with_base_select(base_select)
        .with_action_query(action_query),
        base,
    })
}

#[post("/room-templates/<tpl_id>?<base>", data = "<tpl_form>")]
#[tracing::instrument(skip(redirect_to, tpl_form, index_manager, ctx, session))]
async fn edit_tpl_submit<'a>(
    redirect_to: &RedirectTo,
    tpl_id: RoomTemplateId,
    base: Option<&str>,
    mut tpl_form: Form<CreateTplForm<'a>>,
    ctx: &State<Context>,
    index_manager: &State<IndexManager>,
    session: LoggedInSession,
) -> Result<Redirect> {
    redirect_to.set(&format!("/room-templates/{tpl_id}"));

    let mut conn = ctx.db_pool.get().await?;
    let tpl = db::get_room_template_by_id(tpl_id, &mut conn).await?;
    let is_my_tpl = session.0.is_admin || session.0.user_id == Some(tpl.settings.author_id);
    if !is_my_tpl {
        return Err(anyhow::anyhow!("You're not allowed to edit this room template").into());
    }

    validate_tpl_form(&mut tpl_form)?;

    let named = template_base(index_manager, base, tpl.ap_version.as_deref()).await?;
    let room_manifest = {
        let ap_version = index_manager.base_or_default(named.as_ref()).await;
        let index = index_manager.index_for(&ap_version).await?;
        manifest_from_form(&tpl_form.room.me, &index)
    }?;

    let new_tpl = NewRoomTemplate {
        id: tpl_id,
        name: tpl_form.room.room_name.trim(),
        description: tpl_form.room.room_description.trim(),
        close_date: parse_date(tpl_form.room.close_date, tpl_form.room.tz_offset)?.naive_utc(),
        room_url: tpl_form.room.room_url,
        author_id: None, // (Skips updating that field)
        yaml_validation: tpl_form.room.yaml_validation,
        allow_unsupported: tpl_form.room.allow_unsupported,
        yaml_limit_per_user: tpl_form
            .room
            .yaml_limit_per_user
            .then_some(tpl_form.room.yaml_limit_per_user_nb),
        yaml_limit_bypass_list: tpl_form
            .room
            .yaml_limit_bypass_list
            .split(',')
            .filter_map(|id| i64::from_str(id).ok())
            .collect(),
        manifest: db::Json(room_manifest),
        show_apworlds: tpl_form.room.show_apworlds,
        tpl_name: tpl_form.tpl_name,
        global: tpl_form.tpl_global && session.0.is_admin,
        meta_file: tpl_form.room.meta_file.clone(),
        is_bundle_room: tpl_form.room.is_bundle_room,
        locked: tpl_form.room.locked,
        ap_version: Some(named.map(ApVersion::from)),
    };

    db::update_room_template(&new_tpl, &mut conn).await?;

    Ok(Redirect::to("/room-templates"))
}

#[get("/room-templates/<tpl_id>/delete")]
#[tracing::instrument(skip(ctx, session))]
async fn delete_template(
    ctx: &State<Context>,
    tpl_id: RoomTemplateId,
    session: LoggedInSession,
) -> Result<Redirect> {
    let mut conn = ctx.db_pool.get().await?;
    let tpl = db::get_room_template_by_id(tpl_id, &mut conn).await?;
    let is_my_tpl = session.0.is_admin || session.0.user_id == Some(tpl.settings.author_id);

    if !is_my_tpl {
        return Err(anyhow::anyhow!("You're not allowed to delete this room template").into());
    }

    db::delete_room_template(tpl_id, &mut conn).await?;

    Ok(Redirect::to("/room-templates"))
}

#[get("/room-templates/<tpl_id>/rooms?<page>")]
#[tracing::instrument(skip(ctx, session))]
pub async fn list_associated_rooms<'a>(
    ctx: &State<Context>,
    tpl_id: RoomTemplateId,
    page: Option<u64>,
    session: LoggedInSession,
    lobby_config: &State<LobbyConfig>,
) -> Result<AssociatedRoomsTpl<'a>> {
    let mut conn = ctx.db_pool.get().await?;
    let tpl = db::get_room_template_by_id(tpl_id, &mut conn).await?;
    let is_my_tpl =
        tpl.global || session.0.is_admin || session.0.user_id == Some(tpl.settings.author_id);

    if !is_my_tpl {
        return Err(anyhow::anyhow!("Couldn't find the given template").into());
    }

    let current_page = page.unwrap_or(1);
    let (rooms, max_pages) =
        db::list_rooms_from_template(tpl_id, session.user_id(), current_page, &mut conn).await?;
    Ok(AssociatedRoomsTpl {
        base: TplContext::from_session(
            "template",
            session.0,
            ctx,
            lobby_config,
            Some(format!(
                "Room Templates - {} - Associated Rooms",
                tpl.tpl_name
            )),
        )
        .await,
        tpl,
        rooms,
        current_page,
        max_pages,
    })
}

pub fn validate_tpl_form(tpl_form: &mut CreateTplForm<'_>) -> Result<()> {
    validate_room_form(&mut tpl_form.room)?;

    if tpl_form.tpl_name.trim().is_empty() {
        return Err(anyhow::anyhow!("The template name shouldn't be empty").into());
    }

    Ok(())
}

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        list_templates,
        create_template,
        edit_template,
        delete_template,
        create_tpl_submit,
        edit_tpl_submit,
        list_associated_rooms,
    ]
}
