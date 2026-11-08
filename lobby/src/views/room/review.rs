//! A room's review tab, where filters flag YAMLs for its reviewers to look at.
//!
//! Who can see it is up to `Room::can_review`. The filters are those of the `review-filters`
//! crate: the builtin ones, which are on unless the room's reviewers turned them off, and the
//! ones the reviewers defined for the room. Nothing is stored about the YAMLs, the filters are
//! evaluated every time the tab is loaded.

use anyhow::anyhow;
use askama::Template;
use askama_web::WebTemplate;
use chrono::NaiveDateTime;
use http::header::CONTENT_DISPOSITION;
use rocket::data::{Data, ToByteUnit};
use rocket::http::{Header, Status};
use rocket::serde::json::Json;
use rocket::{delete, get, post, put, State};
use serde::{Deserialize, Serialize};

use review_filters::{
    builtin_rules, evaluate_yaml, ActiveFilter, Filter, FilterExport, FilterRef, RoomYaml,
    YamlEvaluation,
};

use crate::db::{self, ReviewEditor, ReviewFilter, Room, RoomId, YamlId};
use crate::error::{ApiError, ApiResult, RedirectTo, Result, WithContext, WithStatus};
use crate::session::Session;
use crate::{Context, LobbyConfig, TplContext};

/// How many filters reviewers can define for a room
const MAX_FILTERS_PER_ROOM: usize = 500;

/// How big a filter can be, as the editor sends it
const MAX_FILTER_BODY_KIB: u64 = 64;

/// How big an import can be
const MAX_IMPORT_BODY_KIB: u64 = 1024;

#[derive(Template, WebTemplate)]
#[template(path = "room/review.html")]
struct ReviewTpl<'a> {
    base: TplContext<'a>,
    room: Room,
    is_my_room: bool,
}

#[get("/room/<room_id>/review")]
#[tracing::instrument(skip(redirect_to, session, ctx, lobby_config))]
async fn review_page<'a>(
    room_id: RoomId,
    redirect_to: &RedirectTo,
    session: Session,
    ctx: &State<Context>,
    lobby_config: &State<LobbyConfig>,
) -> Result<ReviewTpl<'a>> {
    redirect_to.set(&format!("/room/{room_id}"));

    let mut conn = ctx.db_pool.get().await?;
    let room = db::get_room(room_id, &mut conn).await?;

    if !session.is_logged_in && !session.is_admin {
        return Err(anyhow!("Log in to review this room's YAMLs").into());
    }
    if !room.can_review(session.is_admin, session.user_id) {
        return Err(anyhow!("You aren't a reviewer of this room").into());
    }

    let is_my_room = session.is_admin || session.user_id == Some(room.settings.author_id);

    Ok(ReviewTpl {
        base: TplContext::from_session(
            "room",
            session,
            ctx,
            lobby_config,
            Some(format!("{} - Review", room.settings.name)),
        )
        .await,
        room,
        is_my_room,
    })
}

/// The room, if the session can review it
async fn reviewable_room(
    room_id: RoomId,
    session: &Session,
    conn: &mut diesel_async::AsyncPgConnection,
) -> ApiResult<Room> {
    let room = db::get_room(room_id, conn)
        .await
        .context("Couldn't find the room")
        .status(Status::NotFound)?;

    if !room.can_review(session.is_admin, session.user_id) {
        return Err(ApiError {
            error: anyhow!("You aren't a reviewer of this room"),
            status: Status::Forbidden,
        });
    }

    Ok(room)
}

/// Who is changing filters. Admins logged in with the admin token have no Discord account.
async fn editor(
    session: &Session,
    conn: &mut diesel_async::AsyncPgConnection,
) -> ApiResult<(i64, String)> {
    match session.user_id {
        Some(user_id) => {
            let name = db::get_username(user_id, conn)
                .await?
                .unwrap_or_else(|| "unknown".to_string());
            Ok((user_id, name))
        }
        None => Ok((0, "Admin".to_string())),
    }
}

fn bad_request(error: impl Into<anyhow::Error>) -> ApiError {
    ApiError {
        error: error.into(),
        status: Status::BadRequest,
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError {
        error: anyhow!("Couldn't find this {what} in this room"),
        status: Status::NotFound,
    }
}

async fn read_body(data: Data<'_>, limit_kib: u64) -> ApiResult<String> {
    let body = data
        .open(limit_kib.kibibytes())
        .into_string()
        .await
        .map_err(bad_request)?;
    if !body.is_complete() {
        return Err(bad_request(anyhow!(
            "That's too big, this can't take more than {limit_kib} KiB"
        )));
    }
    Ok(body.into_inner())
}

/// Reads one filter, as the editor sends it or as a builtin file has it. Validating compiles
/// its regexes, which is kept off the async workers.
async fn read_filter(data: Data<'_>) -> ApiResult<Filter> {
    let body = read_body(data, MAX_FILTER_BODY_KIB).await?;
    tokio::task::spawn_blocking(move || {
        let mut filter: Filter =
            serde_json::from_str(&body).map_err(|e| bad_request(anyhow!("Invalid filter: {e}")))?;
        filter.normalize();
        filter.validate().map_err(|e| bad_request(anyhow!(e)))?;
        Ok(filter)
    })
    .await?
}

/// How many more filters fit in the room
async fn room_for_filters(
    room_id: RoomId,
    conn: &mut diesel_async::AsyncPgConnection,
) -> ApiResult<usize> {
    let existing = db::count_review_filters(room_id, conn).await? as usize;
    let room_for = MAX_FILTERS_PER_ROOM.saturating_sub(existing);
    if room_for == 0 {
        return Err(bad_request(anyhow!(
            "A room can't have more than {MAX_FILTERS_PER_ROOM} filters"
        )));
    }
    Ok(room_for)
}

fn filter_to_db(filter: &Filter) -> ApiResult<serde_json::Value> {
    Ok(serde_json::to_value(filter)?)
}

#[derive(Serialize)]
struct BuiltinFilterInfo {
    id: String,
    name: String,
    description: String,
    enabled: bool,
}

#[derive(Serialize)]
struct CustomFilterInfo {
    id: i32,
    enabled: bool,
    filter: serde_json::Value,
    /// Why the filter can't be read, when it can't. It's left out of the evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    last_edited_by_name: String,
    last_edited_at: NaiveDateTime,
}

impl From<ReviewFilter> for CustomFilterInfo {
    fn from(f: ReviewFilter) -> Self {
        let error = serde_json::from_value::<Filter>(f.filter.clone())
            .err()
            .map(|e| e.to_string());
        Self {
            id: f.id,
            enabled: f.enabled,
            filter: f.filter,
            error,
            last_edited_by_name: f.last_edited_by_name,
            last_edited_at: f.last_edited_at,
        }
    }
}

#[derive(Serialize)]
struct YamlReview {
    id: YamlId,
    player_name: String,
    owner_name: String,
    /// As a string, JavaScript can't hold Discord IDs as numbers
    owner_id: String,
    #[serde(flatten)]
    evaluation: YamlEvaluation,
}

#[derive(Serialize)]
struct ReviewData {
    builtins: Vec<BuiltinFilterInfo>,
    filters: Vec<CustomFilterInfo>,
    yamls: Vec<YamlReview>,
}

/// Everything the review tab shows: the filters, and what they flag in each YAML
#[get("/api/room/<room_id>/review")]
#[tracing::instrument(skip(session, ctx))]
async fn review_data(
    room_id: RoomId,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<Json<ReviewData>> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    let disabled_builtins = db::get_review_disabled_builtins(room_id, &mut conn).await?;
    let stored_filters = db::get_review_filters(room_id, &mut conn).await?;
    let yamls = db::get_yamls_for_room_with_owner_names(room_id, &mut conn).await?;
    drop(conn);

    let builtins = builtin_rules()
        .iter()
        .map(|rule| BuiltinFilterInfo {
            id: rule.id().to_string(),
            name: rule.name().to_string(),
            description: rule.description().to_string(),
            enabled: !disabled_builtins.iter().any(|id| id == rule.id()),
        })
        .collect::<Vec<_>>();

    let filters = stored_filters
        .into_iter()
        .map(CustomFilterInfo::from)
        .collect::<Vec<_>>();

    let enabled_builtin_ids = builtins
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.id.clone())
        .collect::<Vec<_>>();
    let enabled_filters = filters
        .iter()
        .filter(|f| f.enabled && f.error.is_none())
        .filter_map(|f| {
            serde_json::from_value::<Filter>(f.filter.clone())
                .ok()
                .map(|filter| (f.id, filter))
        })
        .collect::<Vec<_>>();

    let reviews = tokio::task::spawn_blocking(move || {
        let mut active = builtin_rules()
            .iter()
            .filter(|rule| enabled_builtin_ids.iter().any(|id| id == rule.id()))
            .map(|rule| ActiveFilter {
                id: rule.id().to_string(),
                builtin: true,
                filter: FilterRef::Builtin(rule.as_ref()),
            })
            .collect::<Vec<_>>();
        active.extend(enabled_filters.iter().map(|(id, filter)| ActiveFilter {
            id: id.to_string(),
            builtin: false,
            filter: FilterRef::Custom(filter),
        }));

        let room_yamls = yamls
            .iter()
            .map(|(yaml, _)| RoomYaml {
                player_name: yaml.player_name.clone(),
            })
            .collect::<Vec<_>>();

        yamls
            .iter()
            .map(|(yaml, owner_name)| YamlReview {
                id: yaml.id,
                player_name: yaml.player_name.clone(),
                owner_name: owner_name.clone(),
                owner_id: yaml.owner_id.to_string(),
                evaluation: evaluate_yaml(
                    yaml.current_content(),
                    &yaml.game,
                    &yaml.player_name,
                    &active,
                    &room_yamls,
                ),
            })
            .collect::<Vec<_>>()
    })
    .await?;

    Ok(Json(ReviewData {
        builtins,
        filters,
        yamls: reviews,
    }))
}

#[post("/api/room/<room_id>/review/filters", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn create_filter(
    room_id: RoomId,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<Json<CustomFilterInfo>> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;
    room_for_filters(room_id, &mut conn).await?;
    let filter = read_filter(data).await?;

    add_filters(room_id, vec![filter], &session, &mut conn)
        .await?
        .pop()
        .ok_or_else(|| anyhow!("The filter wasn't added").into())
        .map(Json)
}

async fn add_filters(
    room_id: RoomId,
    filters: Vec<Filter>,
    session: &Session,
    conn: &mut diesel_async::AsyncPgConnection,
) -> ApiResult<Vec<CustomFilterInfo>> {
    // Again, the room may have filled up in the meantime
    let room_for = room_for_filters(room_id, conn).await?;
    if filters.len() > room_for {
        return Err(bad_request(anyhow!(
            "Only {room_for} more filters fit in the room, a room can't have more than {MAX_FILTERS_PER_ROOM}"
        )));
    }

    let values = filters
        .iter()
        .map(filter_to_db)
        .collect::<ApiResult<Vec<_>>>()?;
    let (user_id, name) = editor(session, conn).await?;
    let editor = ReviewEditor {
        user_id,
        name: &name,
    };

    Ok(db::add_review_filters(room_id, values, &editor, conn)
        .await?
        .into_iter()
        .map(CustomFilterInfo::from)
        .collect())
}

#[put("/api/room/<room_id>/review/filters/<filter_id>", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn update_filter(
    room_id: RoomId,
    filter_id: i32,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<Json<CustomFilterInfo>> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;
    let filter = read_filter(data).await?;

    let (user_id, name) = editor(&session, &mut conn).await?;
    let editor = ReviewEditor {
        user_id,
        name: &name,
    };
    let updated = db::update_review_filter(
        room_id,
        filter_id,
        filter_to_db(&filter)?,
        &editor,
        &mut conn,
    )
    .await?
    .ok_or_else(|| not_found("filter"))?;

    Ok(Json(updated.into()))
}

#[derive(Deserialize)]
struct EnabledRequest {
    enabled: bool,
}

#[put(
    "/api/room/<room_id>/review/filters/<filter_id>/enabled",
    data = "<request>"
)]
#[tracing::instrument(skip(session, ctx, request))]
async fn set_filter_enabled(
    room_id: RoomId,
    filter_id: i32,
    session: Session,
    request: Json<EnabledRequest>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    if !db::set_review_filter_enabled(room_id, filter_id, request.enabled, &mut conn).await? {
        return Err(not_found("filter"));
    }
    Ok(())
}

#[delete("/api/room/<room_id>/review/filters/<filter_id>")]
#[tracing::instrument(skip(session, ctx))]
async fn delete_filter(
    room_id: RoomId,
    filter_id: i32,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    if !db::delete_review_filter(room_id, filter_id, &mut conn).await? {
        return Err(not_found("filter"));
    }
    Ok(())
}

/// Takes the IDs of every one of the room's filters, in their new order
#[put("/api/room/<room_id>/review/filter-order", data = "<filter_ids>")]
#[tracing::instrument(skip(session, ctx))]
async fn reorder_filters(
    room_id: RoomId,
    session: Session,
    filter_ids: Json<Vec<i32>>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    db::reorder_review_filters(room_id, filter_ids.into_inner(), &mut conn)
        .await
        .status(Status::Conflict)?;
    Ok(())
}

#[put(
    "/api/room/<room_id>/review/builtins/<builtin_id>/enabled",
    data = "<request>"
)]
#[tracing::instrument(skip(session, ctx, request))]
async fn set_builtin_enabled(
    room_id: RoomId,
    builtin_id: &str,
    session: Session,
    request: Json<EnabledRequest>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    if !builtin_rules().iter().any(|rule| rule.id() == builtin_id) {
        return Err(ApiError {
            error: anyhow!("There's no builtin filter called {builtin_id}"),
            status: Status::NotFound,
        });
    }

    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    db::set_review_builtin_enabled(room_id, builtin_id, request.enabled, &mut conn).await?;
    Ok(())
}

#[derive(rocket::Responder)]
#[response(content_type = "json")]
struct JsonDownload {
    body: String,
    disposition: Header<'static>,
}

impl JsonDownload {
    fn new(value: &impl Serialize, file_name: &str) -> ApiResult<Self> {
        Ok(Self {
            body: serde_json::to_string_pretty(value)?,
            disposition: Header::new(
                CONTENT_DISPOSITION.as_str(),
                format!("attachment; filename=\"{file_name}\""),
            ),
        })
    }
}

/// A file name made of a filter's name, which is also what a builtin file would be called
fn file_stem(name: &str) -> String {
    let mut stem = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            stem.push(c.to_ascii_lowercase());
        } else if !stem.is_empty() && !stem.ends_with('_') {
            stem.push('_');
        }
    }
    let stem = stem.trim_end_matches('_');
    if stem.is_empty() {
        "filter".to_string()
    } else {
        stem.to_string()
    }
}

/// The room's filters, in the format `import_filters` takes
#[get("/api/room/<room_id>/review/export")]
#[tracing::instrument(skip(session, ctx))]
async fn export_filters(
    room_id: RoomId,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<JsonDownload> {
    let mut conn = ctx.db_pool.get().await?;
    let room = reviewable_room(room_id, &session, &mut conn).await?;

    let filters = db::get_review_filters(room_id, &mut conn)
        .await?
        .into_iter()
        .filter_map(|f| serde_json::from_value::<Filter>(f.filter).ok())
        .collect::<Vec<_>>();

    JsonDownload::new(
        &FilterExport::new(filters),
        &format!("{}_review_filters.json", file_stem(&room.settings.name)),
    )
}

/// One filter, in the format of the builtin files
#[get("/api/room/<room_id>/review/filters/<filter_id>/export")]
#[tracing::instrument(skip(session, ctx))]
async fn export_filter(
    room_id: RoomId,
    filter_id: i32,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<JsonDownload> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    let stored = db::get_review_filter(room_id, filter_id, &mut conn)
        .await?
        .ok_or_else(|| not_found("filter"))?;
    let filter = serde_json::from_value::<Filter>(stored.filter)
        .map_err(|e| bad_request(anyhow!("This filter can't be read: {e}")))?;

    JsonDownload::new(&filter, &format!("{}.json", file_stem(&filter.rule.name)))
}

/// Adds filters after the room's own: an export, one filter, or a list of them. Nothing is
/// added unless every filter is valid.
#[post("/api/room/<room_id>/review/import", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn import_filters(
    room_id: RoomId,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<Json<Vec<CustomFilterInfo>>> {
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;
    let room_for = room_for_filters(room_id, &mut conn).await?;

    let body = read_body(data, MAX_IMPORT_BODY_KIB).await?;
    // Counted before they're validated, which compiles their regexes
    let filters = tokio::task::spawn_blocking(move || {
        review_filters::parse_import(&body, room_for).map_err(|e| bad_request(anyhow!(e)))
    })
    .await??;
    if filters.is_empty() {
        return Err(bad_request(anyhow!("There are no filters in this file")));
    }

    Ok(Json(
        add_filters(room_id, filters, &session, &mut conn).await?,
    ))
}

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        review_page,
        review_data,
        create_filter,
        update_filter,
        set_filter_enabled,
        delete_filter,
        reorder_filters,
        set_builtin_enabled,
        export_filters,
        export_filter,
        import_filters,
    ]
}

#[cfg(test)]
mod tests {
    use super::file_stem;

    #[test]
    fn test_file_stem() {
        assert_eq!(file_stem("Death link"), "death_link");
        assert_eq!(
            file_stem("  No cross-player plando! "),
            "no_cross_player_plando"
        );
        assert_eq!(file_stem("Héllo wörld"), "h_llo_w_rld");
        assert_eq!(file_stem("!!!"), "filter");
    }
}
