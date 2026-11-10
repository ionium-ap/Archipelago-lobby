//! A room's review tab, where filters flag YAMLs for its reviewers to look at.
//!
//! Who can see it is up to `Room::can_review`. The filters are those of the `review-filters`
//! crate: the builtin ones, which are on unless the room's reviewers turned them off, and the
//! ones the reviewers defined for the room. The filters are evaluated every time the tab is
//! loaded. What's stored about a YAML is what reviewers make of it (a mark, see
//! `ReviewStatus`) and what they say about it (comments), none of which players see.

use std::collections::HashMap;

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

use crate::db::{self, ReviewEditor, ReviewFilter, ReviewStatus, Room, RoomId, YamlId};
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

/// How big the other requests can be
const MAX_REQUEST_BODY_KIB: u64 = 64;

/// Reads a JSON request. Rocket's `Json` guard isn't used: what it refuses goes to the lobby's
/// catchers, which redirect, and `fetch` takes the page it's redirected to for a success.
async fn read_json<T: serde::de::DeserializeOwned>(data: Data<'_>) -> ApiResult<T> {
    let body = read_body(data, MAX_REQUEST_BODY_KIB).await?;
    serde_json::from_str(&body).map_err(|e| bad_request(anyhow!("Invalid request: {e}")))
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

impl CustomFilterInfo {
    /// Reads a stored filter once, for both what the page shows and what's evaluated: the
    /// filter comes back when it can be read, and `error` says why when it can't.
    fn parse(f: ReviewFilter) -> (Self, Option<Filter>) {
        let (parsed, error) = match Filter::deserialize(&f.filter) {
            Ok(filter) => (Some(filter), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let info = Self {
            id: f.id,
            enabled: f.enabled,
            filter: f.filter,
            error,
            last_edited_by_name: f.last_edited_by_name,
            last_edited_at: f.last_edited_at,
        };
        (info, parsed)
    }
}

impl From<ReviewFilter> for CustomFilterInfo {
    fn from(f: ReviewFilter) -> Self {
        Self::parse(f).0
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
    /// Of the content the YAML has now, for marks to say which version they're about
    content_sha256: String,
    /// What a reviewer made of it, if one did
    mark: Option<MarkInfo>,
    comment_count: i64,
}

#[derive(Serialize)]
struct MarkInfo {
    status: ReviewStatus,
    marked_by_name: String,
    marked_at: NaiveDateTime,
    /// Whether the YAML's content changed since it was marked
    changed: bool,
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
    let mut marks = db::get_review_marks(room_id, &mut conn)
        .await?
        .into_iter()
        .map(|mark| (mark.yaml_id, mark))
        .collect::<HashMap<_, _>>();
    let comment_counts = db::count_review_comments(room_id, &mut conn)
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();
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

    let (filters, parsed): (Vec<_>, Vec<_>) = stored_filters
        .into_iter()
        .map(CustomFilterInfo::parse)
        .unzip();

    let enabled_builtin_ids = builtins
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.id.clone())
        .collect::<Vec<_>>();
    // The ones that can't be read are left out, as their `error` tells the page
    let enabled_filters = filters
        .iter()
        .zip(parsed)
        .filter(|(info, _)| info.enabled)
        .filter_map(|(info, filter)| Some((info.id, filter?)))
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
            .map(|(yaml, owner_name)| {
                let sha256 = content_sha256(yaml.current_content());
                let mark = marks.remove(&yaml.id).and_then(|mark| {
                    Some(MarkInfo {
                        status: ReviewStatus::from_db(&mark.status)?,
                        changed: mark.content_sha256 != sha256,
                        marked_by_name: mark.marked_by_name,
                        marked_at: mark.marked_at,
                    })
                });
                YamlReview {
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
                    content_sha256: sha256,
                    mark,
                    comment_count: comment_counts.get(&yaml.id).copied().unwrap_or(0),
                }
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
    data = "<data>"
)]
#[tracing::instrument(skip(session, ctx, data))]
async fn set_filter_enabled(
    room_id: RoomId,
    filter_id: i32,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let request: EnabledRequest = read_json(data).await?;
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
#[put("/api/room/<room_id>/review/filter-order", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn reorder_filters(
    room_id: RoomId,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let filter_ids: Vec<i32> = read_json(data).await?;
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;

    db::reorder_review_filters(room_id, filter_ids, &mut conn)
        .await
        .status(Status::Conflict)?;
    Ok(())
}

#[put(
    "/api/room/<room_id>/review/builtins/<builtin_id>/enabled",
    data = "<data>"
)]
#[tracing::instrument(skip(session, ctx, data))]
async fn set_builtin_enabled(
    room_id: RoomId,
    builtin_id: &str,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let request: EnabledRequest = read_json(data).await?;
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

/// What a mark remembers of the YAML's content, to tell whether it changed since
fn content_sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The YAML, if it's in the room
async fn yaml_in_room(
    room_id: RoomId,
    yaml_id: YamlId,
    conn: &mut diesel_async::AsyncPgConnection,
) -> ApiResult<db::Yaml> {
    db::get_yaml_in_room(room_id, yaml_id, conn)
        .await
        .map_err(|_| not_found("YAML"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkRequest {
    /// `null` takes the mark away. It has to be there: a request that leaves it out by
    /// mistake shouldn't take a mark away.
    #[serde(deserialize_with = "Option::deserialize")]
    status: Option<ReviewStatus>,
    /// The `content_sha256` of the YAML as the page that marks it has it, so that what's
    /// marked is what the reviewer looked at. Needed to mark, not to take a mark away.
    #[serde(default)]
    content_sha256: Option<String>,
}

#[put("/api/room/<room_id>/review/yamls/<yaml_id>/mark", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn set_mark(
    room_id: RoomId,
    yaml_id: YamlId,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let request: MarkRequest = read_json(data).await?;
    let mut conn = ctx.db_pool.get().await?;
    reviewable_room(room_id, &session, &mut conn).await?;
    let yaml = yaml_in_room(room_id, yaml_id, &mut conn).await?;

    let current_sha256 = content_sha256(yaml.current_content());
    if request.status.is_some() {
        let Some(seen_sha256) = &request.content_sha256 else {
            return Err(bad_request(anyhow!(
                "Say which version of the YAML is marked, with content_sha256"
            )));
        };
        if *seen_sha256 != current_sha256 {
            return Err(ApiError {
                error: anyhow!(
                    "The YAML changed since the page was loaded. Have a look at it again before marking it."
                ),
                status: Status::Conflict,
            });
        }
    }

    let (user_id, name) = editor(&session, &mut conn).await?;
    let editor = ReviewEditor {
        user_id,
        name: &name,
    };
    db::set_review_mark(
        room_id,
        yaml_id,
        request.status,
        &current_sha256,
        &editor,
        &mut conn,
    )
    .await?;
    Ok(())
}

/// How long a comment can be, in characters
const MAX_COMMENT_LEN: usize = 5000;

/// How many comments a YAML can have
const MAX_COMMENTS_PER_YAML: usize = 200;

#[derive(Serialize)]
struct CommentInfo {
    id: i32,
    author_name: String,
    content: String,
    created_at: NaiveDateTime,
    edited_at: Option<NaiveDateTime>,
    can_edit: bool,
    can_delete: bool,
}

/// Who can do what with a comment: its author edits and deletes it, the room's owner and
/// admins can delete it too
struct CommentRights {
    user_id: i64,
    moderates: bool,
}

impl CommentRights {
    fn new(session: &Session, room: &Room, user_id: i64) -> Self {
        Self {
            user_id,
            moderates: session.is_admin || session.user_id == Some(room.settings.author_id),
        }
    }

    fn info(&self, comment: db::ReviewComment) -> CommentInfo {
        let can_edit = comment.author_id == self.user_id;
        CommentInfo {
            id: comment.id,
            author_name: comment.author_name,
            content: comment.content,
            created_at: comment.created_at,
            edited_at: comment.edited_at,
            can_edit,
            can_delete: can_edit || self.moderates,
        }
    }
}

#[derive(Deserialize)]
struct CommentRequest {
    content: String,
}

fn comment_content(request: &CommentRequest) -> ApiResult<&str> {
    let content = request.content.trim();
    if content.is_empty() {
        return Err(bad_request(anyhow!("A comment can't be empty")));
    }
    if content.chars().count() > MAX_COMMENT_LEN {
        return Err(bad_request(anyhow!(
            "A comment can't be longer than {MAX_COMMENT_LEN} characters"
        )));
    }
    Ok(content)
}

#[get("/api/room/<room_id>/review/yamls/<yaml_id>/comments")]
#[tracing::instrument(skip(session, ctx))]
async fn list_comments(
    room_id: RoomId,
    yaml_id: YamlId,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<Json<Vec<CommentInfo>>> {
    let mut conn = ctx.db_pool.get().await?;
    let room = reviewable_room(room_id, &session, &mut conn).await?;
    yaml_in_room(room_id, yaml_id, &mut conn).await?;

    let (user_id, _) = editor(&session, &mut conn).await?;
    let rights = CommentRights::new(&session, &room, user_id);
    Ok(Json(
        db::get_review_comments(room_id, yaml_id, &mut conn)
            .await?
            .into_iter()
            .map(|comment| rights.info(comment))
            .collect(),
    ))
}

#[post("/api/room/<room_id>/review/yamls/<yaml_id>/comments", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn add_comment(
    room_id: RoomId,
    yaml_id: YamlId,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<Json<CommentInfo>> {
    let request: CommentRequest = read_json(data).await?;
    let content = comment_content(&request)?;

    let mut conn = ctx.db_pool.get().await?;
    let room = reviewable_room(room_id, &session, &mut conn).await?;
    yaml_in_room(room_id, yaml_id, &mut conn).await?;

    let existing = db::count_yaml_review_comments(room_id, yaml_id, &mut conn).await? as usize;
    if existing >= MAX_COMMENTS_PER_YAML {
        return Err(bad_request(anyhow!(
            "A YAML can't have more than {MAX_COMMENTS_PER_YAML} comments"
        )));
    }

    let (user_id, name) = editor(&session, &mut conn).await?;
    let editor = ReviewEditor {
        user_id,
        name: &name,
    };
    let comment = db::add_review_comment(room_id, yaml_id, content, &editor, &mut conn).await?;
    Ok(Json(
        CommentRights::new(&session, &room, user_id).info(comment),
    ))
}

#[put("/api/room/<room_id>/review/comments/<comment_id>", data = "<data>")]
#[tracing::instrument(skip(session, ctx, data))]
async fn update_comment(
    room_id: RoomId,
    comment_id: i32,
    session: Session,
    data: Data<'_>,
    ctx: &State<Context>,
) -> ApiResult<Json<CommentInfo>> {
    let request: CommentRequest = read_json(data).await?;
    let content = comment_content(&request)?;

    let mut conn = ctx.db_pool.get().await?;
    let room = reviewable_room(room_id, &session, &mut conn).await?;
    let (user_id, _) = editor(&session, &mut conn).await?;
    let rights = CommentRights::new(&session, &room, user_id);

    let comment = db::get_review_comment(room_id, comment_id, &mut conn)
        .await?
        .ok_or_else(|| not_found("comment"))?;
    if !rights.info(comment).can_edit {
        return Err(ApiError {
            error: anyhow!("Only the author of a comment can edit it"),
            status: Status::Forbidden,
        });
    }

    let comment = db::update_review_comment(room_id, comment_id, content, &mut conn)
        .await?
        .ok_or_else(|| not_found("comment"))?;
    Ok(Json(rights.info(comment)))
}

#[delete("/api/room/<room_id>/review/comments/<comment_id>")]
#[tracing::instrument(skip(session, ctx))]
async fn delete_comment(
    room_id: RoomId,
    comment_id: i32,
    session: Session,
    ctx: &State<Context>,
) -> ApiResult<()> {
    let mut conn = ctx.db_pool.get().await?;
    let room = reviewable_room(room_id, &session, &mut conn).await?;
    let (user_id, _) = editor(&session, &mut conn).await?;

    let comment = db::get_review_comment(room_id, comment_id, &mut conn)
        .await?
        .ok_or_else(|| not_found("comment"))?;
    if !CommentRights::new(&session, &room, user_id)
        .info(comment)
        .can_delete
    {
        return Err(ApiError {
            error: anyhow!("Only the author of a comment, or the room's owner, can delete it"),
            status: Status::Forbidden,
        });
    }

    db::delete_review_comment(room_id, comment_id, &mut conn).await?;
    Ok(())
}

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        set_mark,
        list_comments,
        add_comment,
        update_comment,
        delete_comment,
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
    use super::{file_stem, CustomFilterInfo};
    use crate::db::{ReviewFilter, RoomId};

    fn stored(filter: serde_json::Value) -> ReviewFilter {
        ReviewFilter {
            id: 1,
            room_id: RoomId::new_v4(),
            filter,
            position: 0,
            enabled: true,
            last_edited_by: 1,
            last_edited_by_name: "Reviewer".into(),
            last_edited_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
        }
    }

    #[test]
    fn test_custom_filter_info_parses_once_for_both() {
        let readable = serde_json::json!({
            "name": "Death link",
            "then": {"type": "check", "path": "death_link", "check": {"type": "exists"}},
            "severity": "info",
        });
        let (info, filter) = CustomFilterInfo::parse(stored(readable.clone()));
        assert!(info.error.is_none());
        assert_eq!(info.filter, readable);
        assert_eq!(filter.unwrap().rule.name, "Death link");

        let unreadable = serde_json::json!({"name": "Old shape", "rule": "gone"});
        let (info, filter) = CustomFilterInfo::parse(stored(unreadable.clone()));
        assert!(info.error.is_some());
        assert_eq!(info.filter, unreadable);
        assert!(filter.is_none());
    }

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
