//! The review filters reviewers define for a room, and the builtin ones they turned off

use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::db::RoomId;
use crate::error::Result;
use crate::schema::{room_review_filters, rooms};

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = room_review_filters)]
pub struct ReviewFilter {
    pub id: i32,
    pub room_id: RoomId,
    /// A `review_filters::Filter`. It's kept as JSON here so that a filter that the lobby
    /// can't read anymore doesn't keep the others from being listed.
    pub filter: serde_json::Value,
    pub position: i32,
    pub enabled: bool,
    pub last_edited_by: i64,
    pub last_edited_by_name: String,
    pub last_edited_at: NaiveDateTime,
}

/// Who changed a filter
#[derive(Debug, Clone)]
pub struct ReviewEditor<'a> {
    pub user_id: i64,
    pub name: &'a str,
}

#[derive(Insertable)]
#[diesel(table_name = room_review_filters)]
struct NewReviewFilter<'a> {
    room_id: RoomId,
    filter: serde_json::Value,
    position: i32,
    enabled: bool,
    last_edited_by: i64,
    last_edited_by_name: &'a str,
    last_edited_at: NaiveDateTime,
}

#[tracing::instrument(skip(conn))]
pub async fn get_review_filters(
    room_id: RoomId,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<ReviewFilter>> {
    Ok(room_review_filters::table
        .filter(room_review_filters::room_id.eq(room_id))
        .order_by((
            room_review_filters::position.asc(),
            room_review_filters::id.asc(),
        ))
        .select(ReviewFilter::as_select())
        .get_results(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn count_review_filters(room_id: RoomId, conn: &mut AsyncPgConnection) -> Result<i64> {
    Ok(room_review_filters::table
        .filter(room_review_filters::room_id.eq(room_id))
        .count()
        .get_result(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn get_review_filter(
    room_id: RoomId,
    filter_id: i32,
    conn: &mut AsyncPgConnection,
) -> Result<Option<ReviewFilter>> {
    Ok(room_review_filters::table
        .filter(room_review_filters::room_id.eq(room_id))
        .filter(room_review_filters::id.eq(filter_id))
        .select(ReviewFilter::as_select())
        .first(conn)
        .await
        .optional()?)
}

/// Adds filters after the room's other ones, in the order given
#[tracing::instrument(skip(filters, conn))]
pub async fn add_review_filters(
    room_id: RoomId,
    filters: Vec<serde_json::Value>,
    editor: &ReviewEditor<'_>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<ReviewFilter>> {
    let editor = editor.clone();
    conn.transaction::<_, crate::error::Error, _>(|conn| {
        async move {
            let last_position: Option<i32> = room_review_filters::table
                .filter(room_review_filters::room_id.eq(room_id))
                .select(diesel::dsl::max(room_review_filters::position))
                .first(conn)
                .await?;
            let first_position = last_position.map_or(0, |p| p + 1);
            let now = chrono::Utc::now().naive_utc();

            let new_filters = filters
                .into_iter()
                .enumerate()
                .map(|(i, filter)| NewReviewFilter {
                    room_id,
                    filter,
                    position: first_position + i as i32,
                    enabled: true,
                    last_edited_by: editor.user_id,
                    last_edited_by_name: editor.name,
                    last_edited_at: now,
                })
                .collect::<Vec<_>>();

            Ok(diesel::insert_into(room_review_filters::table)
                .values(&new_filters)
                .returning(ReviewFilter::as_returning())
                .get_results(conn)
                .await?)
        }
        .scope_boxed()
    })
    .await
}

/// Returns the filter as it is now, or `None` when the room has no such filter
#[tracing::instrument(skip(filter, conn))]
pub async fn update_review_filter(
    room_id: RoomId,
    filter_id: i32,
    filter: serde_json::Value,
    editor: &ReviewEditor<'_>,
    conn: &mut AsyncPgConnection,
) -> Result<Option<ReviewFilter>> {
    Ok(diesel::update(room_review_filters::table)
        .filter(room_review_filters::room_id.eq(room_id))
        .filter(room_review_filters::id.eq(filter_id))
        .set((
            room_review_filters::filter.eq(filter),
            room_review_filters::last_edited_by.eq(editor.user_id),
            room_review_filters::last_edited_by_name.eq(editor.name),
            room_review_filters::last_edited_at.eq(chrono::Utc::now().naive_utc()),
        ))
        .returning(ReviewFilter::as_returning())
        .get_result(conn)
        .await
        .optional()?)
}

/// Returns whether the room has that filter
#[tracing::instrument(skip(conn))]
pub async fn set_review_filter_enabled(
    room_id: RoomId,
    filter_id: i32,
    enabled: bool,
    conn: &mut AsyncPgConnection,
) -> Result<bool> {
    let updated = diesel::update(room_review_filters::table)
        .filter(room_review_filters::room_id.eq(room_id))
        .filter(room_review_filters::id.eq(filter_id))
        .set(room_review_filters::enabled.eq(enabled))
        .execute(conn)
        .await?;
    Ok(updated > 0)
}

/// Returns whether the room had that filter
#[tracing::instrument(skip(conn))]
pub async fn delete_review_filter(
    room_id: RoomId,
    filter_id: i32,
    conn: &mut AsyncPgConnection,
) -> Result<bool> {
    let deleted = diesel::delete(room_review_filters::table)
        .filter(room_review_filters::room_id.eq(room_id))
        .filter(room_review_filters::id.eq(filter_id))
        .execute(conn)
        .await?;
    Ok(deleted > 0)
}

/// Puts the room's filters in the order of `filter_ids`, which should list every one of them
#[tracing::instrument(skip(conn))]
pub async fn reorder_review_filters(
    room_id: RoomId,
    filter_ids: Vec<i32>,
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    conn.transaction::<_, crate::error::Error, _>(|conn| {
        async move {
            let mut existing: Vec<i32> = room_review_filters::table
                .filter(room_review_filters::room_id.eq(room_id))
                .select(room_review_filters::id)
                .get_results(conn)
                .await?;
            existing.sort_unstable();
            let mut requested = filter_ids.clone();
            requested.sort_unstable();
            if existing != requested {
                return Err(anyhow::anyhow!(
                    "The filters changed in the meantime, reload the page and try again"
                )
                .into());
            }

            for (position, filter_id) in filter_ids.into_iter().enumerate() {
                diesel::update(room_review_filters::table.find(filter_id))
                    .set(room_review_filters::position.eq(position as i32))
                    .execute(conn)
                    .await?;
            }
            Ok(())
        }
        .scope_boxed()
    })
    .await
}

#[tracing::instrument(skip(conn))]
pub async fn get_review_disabled_builtins(
    room_id: RoomId,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<String>> {
    Ok(rooms::table
        .find(room_id)
        .select(rooms::review_disabled_builtins)
        .first(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn set_review_builtin_enabled(
    room_id: RoomId,
    builtin_id: &str,
    enabled: bool,
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    // Done in SQL so that two reviewers switching different builtins don't undo each other
    let without = diesel::dsl::sql::<diesel::sql_types::Array<diesel::sql_types::Text>>(
        "array_remove(review_disabled_builtins, ",
    )
    .bind::<diesel::sql_types::Text, _>(builtin_id)
    .sql(")");
    let with = diesel::dsl::sql::<diesel::sql_types::Array<diesel::sql_types::Text>>(
        "array_append(array_remove(review_disabled_builtins, ",
    )
    .bind::<diesel::sql_types::Text, _>(builtin_id)
    .sql("), ")
    .bind::<diesel::sql_types::Text, _>(builtin_id)
    .sql(")");

    let query = diesel::update(rooms::table.find(room_id));
    if enabled {
        query
            .set(rooms::review_disabled_builtins.eq(without))
            .execute(conn)
            .await?;
    } else {
        query
            .set(rooms::review_disabled_builtins.eq(with))
            .execute(conn)
            .await?;
    }
    Ok(())
}
