use crate::db::RoomId;
use anyhow::Context;
use apwm::{Index, Manifest};
use chrono::{NaiveDateTime, Timelike};
use diesel::dsl::now;
use diesel::prelude::*;
use diesel::{AsChangeset, Insertable, Queryable, Selectable};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::db::Json;
use crate::error::Result;
use crate::schema::{discord_users, room_info, room_templates, rooms, yamls};

use super::{RoomTemplateId, YamlValidationStatus};

#[derive(Insertable, AsChangeset, Debug)]
#[diesel(table_name=rooms)]
pub struct NewRoom<'a> {
    pub id: RoomId,
    pub name: &'a str,
    pub close_date: NaiveDateTime,
    pub description: &'a str,
    pub room_url: &'a str,
    pub author_id: Option<i64>,
    #[diesel(treat_none_as_null = true)]
    pub yaml_limit_per_user: Option<i32>,
    pub yaml_validation: bool,
    pub allow_unsupported: bool,
    pub yaml_limit_bypass_list: Vec<i64>,
    pub manifest: Json<Manifest>,
    pub show_apworlds: bool,
    pub from_template_id: Option<Option<RoomTemplateId>>,
    pub allow_invalid_yamls: bool,
    pub meta_file: String,
    pub is_bundle_room: bool,
    pub locked: bool,
}

macro_rules! define_settings_structs {
    (
        struct $primary:ident = $primary_table:path,
        struct $secondary:ident = $secondary_table:path,
        {
            $($field:ident : $ty:ty),* $(,)?
        }
    ) => {
        #[derive(Debug, Clone, Queryable, Selectable)]
        #[diesel(table_name = $primary_table)]
        pub struct $primary {
            $(pub $field: $ty,)*
        }

        #[derive(Debug, Clone, Queryable, Selectable)]
        #[diesel(table_name = $secondary_table)]
        pub struct $secondary {
            $(pub $field: $ty,)*
        }

        impl From<$secondary> for $primary {
            fn from(s: $secondary) -> $primary {
                $primary {
                    $($field: s.$field,)*
                }
            }
        }
    };
}

define_settings_structs! {
    struct RoomSettings = rooms,
    struct RoomTemplateSettings = room_templates,
    {
        name: String,
        close_date: NaiveDateTime,
        description: String,
        room_url: String,
        author_id: i64,
        yaml_validation: bool,
        allow_unsupported: bool,
        yaml_limit_per_user: Option<i32>,
        yaml_limit_bypass_list: Vec<i64>,
        manifest: Json<Manifest>,
        show_apworlds: bool,
        created_at: NaiveDateTime,
        updated_at: NaiveDateTime,
        allow_invalid_yamls: bool,
        meta_file: String,
        is_bundle_room: bool,
        locked: bool,
    }
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = rooms)]
pub struct Room {
    pub id: RoomId,
    #[diesel(embed)]
    pub settings: RoomSettings,
    pub from_template_id: Option<RoomTemplateId>,
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = room_templates)]
pub struct RoomTemplate {
    pub id: RoomTemplateId,
    #[diesel(embed)]
    pub settings: RoomTemplateSettings,
    pub global: bool,
    pub tpl_name: String,
}

impl RoomSettings {
    pub fn default(index: &Index) -> Result<Self> {
        Ok(Self {
            name: "".to_string(),
            close_date: Self::default_close_date()?,
            description: "".to_string(),
            room_url: "".to_string(),
            author_id: -1,
            yaml_validation: true,
            allow_unsupported: false,
            yaml_limit_per_user: None,
            yaml_limit_bypass_list: vec![],
            manifest: Json(Manifest::from_index_with_default_versions(index)?),
            show_apworlds: true,
            created_at: Self::default_close_date()?,
            updated_at: Self::default_close_date()?,
            allow_invalid_yamls: false,
            meta_file: "".to_string(),
            is_bundle_room: false,
            locked: false,
        })
    }

    pub fn default_close_date() -> Result<NaiveDateTime> {
        Ok(chrono::Utc::now()
            .naive_utc()
            .with_second(0)
            .context("Failed to create default datetime")?)
    }
}

impl Room {
    pub fn is_closed(&self) -> bool {
        self.settings.close_date < chrono::offset::Utc::now().naive_utc()
    }
}

#[tracing::instrument(skip(conn))]
pub async fn create_room<'a>(
    new_room: &'a NewRoom<'a>,
    conn: &mut AsyncPgConnection,
) -> Result<Room> {
    Ok(diesel::insert_into(rooms::table)
        .values(new_room)
        .returning(Room::as_returning())
        .get_result(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn update_room<'a>(
    new_room: &'a NewRoom<'a>,
    conn: &mut AsyncPgConnection,
) -> Result<Room> {
    if !new_room.yaml_validation {
        diesel::update(yamls::table)
            .filter(yamls::room_id.eq(new_room.id))
            .set((
                yamls::validation_status.eq(YamlValidationStatus::Unknown),
                yamls::apworlds.eq(Vec::<(String, semver::Version)>::new()),
                yamls::last_error.eq(Option::<String>::None),
                yamls::last_validation_time.eq(now),
            ))
            .execute(conn)
            .await?;
    }

    Ok(diesel::update(rooms::table)
        .filter(rooms::id.eq(&new_room.id))
        .set(new_room)
        .returning(Room::as_returning())
        .get_result(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn update_room_manifest(
    room_id: RoomId,
    new_manifest: &Manifest,
    conn: &mut AsyncPgConnection,
) -> Result<()> {
    diesel::update(rooms::table.find(room_id))
        .set(rooms::manifest.eq(Json(new_manifest)))
        .execute(conn)
        .await?;
    Ok(())
}

#[tracing::instrument(skip(conn))]
pub async fn delete_room(room_id: RoomId, conn: &mut AsyncPgConnection) -> Result<()> {
    diesel::delete(rooms::table)
        .filter(rooms::id.eq(room_id))
        .execute(conn)
        .await?;

    Ok(())
}

#[tracing::instrument(skip(conn))]
pub async fn get_room(room_id: RoomId, conn: &mut AsyncPgConnection) -> Result<Room> {
    Ok(rooms::table
        .find(room_id)
        .select(Room::as_select())
        .first::<Room>(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn get_room_and_author(
    room_id: RoomId,
    conn: &mut AsyncPgConnection,
) -> Result<(Room, String, Option<super::RoomInfo>)> {
    use crate::db::RoomInfo as DbRoomInfo;
    Ok(rooms::table
        .find(room_id)
        .inner_join(discord_users::table)
        .left_join(room_info::table)
        .select((
            Room::as_select(),
            discord_users::username,
            Option::<DbRoomInfo>::as_select(),
        ))
        .first(conn)
        .await?)
}

#[tracing::instrument(skip(conn))]
pub async fn get_room_with_info(
    room_id: RoomId,
    conn: &mut AsyncPgConnection,
) -> Result<(Room, Option<super::RoomInfo>)> {
    use crate::db::RoomInfo as DbRoomInfo;
    Ok(rooms::table
        .find(room_id)
        .left_join(room_info::table)
        .select((Room::as_select(), Option::<DbRoomInfo>::as_select()))
        .first(conn)
        .await?)
}
