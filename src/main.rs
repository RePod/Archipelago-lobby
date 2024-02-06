use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{BufReader, Cursor, Write};
use std::path::PathBuf;

use api::{Yaml, YamlFile};
use askama::Template;
use auth::{AdminSession, Session};
use diesel::r2d2::Pool;
use diesel::sqlite::Sqlite;
use diesel::{r2d2::ConnectionManager, SqliteConnection};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use rocket::data::{Limits, ToByteUnit};
use rocket::form::Form;
use rocket::http::hyper::header::CONTENT_DISPOSITION;
use rocket::http::{ContentType, CookieJar, Header};
use rocket::response::Redirect;
use rocket::{get, launch, post, routes, uri, State};
use uuid::Uuid;

use crate::api::Room;
use crate::error::{Error, RedirectTo, Result, WithContext};

mod admin;
mod api;
mod auth;
mod diesel_uuid;
mod error;
mod schema;

pub struct Context {
    db_pool: Pool<ConnectionManager<SqliteConnection>>,
}

struct TplContext<'a> {
    is_admin: bool,
    cur_module: &'a str,
    user_id: Uuid,
    err_msg: Option<String>,
}

impl<'a> TplContext<'a> {
    pub fn from_session(module: &'a str, mut session: Session, cookies: &CookieJar) -> Self {
        let tpl = Self {
            cur_module: module,
            is_admin: session.is_admin,
            user_id: session.user_id,
            err_msg: session.err_msg.take(),
        };

        session
            .save(cookies)
            .expect("Failed to save session somehow");

        tpl
    }
}

#[derive(Template)]
#[template(path = "room.html")]
struct RoomTpl<'a> {
    base: TplContext<'a>,
    room: Room,
    yamls: Vec<Yaml>,
    player_count: usize,
    is_closed: bool,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl<'a> {
    base: TplContext<'a>,
}

#[get("/?<room>")]
fn root<'a>(
    room: Option<Uuid>,
    session: Session,
    cookies: &CookieJar,
) -> Result<either::Either<IndexTpl<'a>, Redirect>> {
    if let Some(room_id) = room {
        return Ok(either::Right(Redirect::to(uri!(room(room_id)))));
    }

    Ok(either::Left(IndexTpl {
        base: TplContext::from_session("index", session, cookies),
    }))
}

#[get("/room/<uuid>")]
fn room<'a>(
    uuid: Uuid,
    ctx: &State<Context>,
    session: Session,
    cookies: &CookieJar,
) -> Result<RoomTpl<'a>> {
    let room = api::get_room(uuid, ctx)?;
    let mut yamls = api::get_yamls_for_room(uuid, ctx)?;
    yamls.sort_by(|a, b| a.game.cmp(&b.game));
    Ok(RoomTpl {
        base: TplContext::from_session("index", session, cookies),
        player_count: yamls.len(),
        is_closed: room.is_closed(),
        room,
        yamls,
    })
}

#[post("/room/<uuid>/upload", data = "<yaml>")]
fn upload_yaml(
    redirect_to: &RedirectTo,
    uuid: Uuid,
    yaml: Form<&[u8]>,
    session: Session,
    ctx: &State<Context>,
) -> Result<Redirect> {
    redirect_to.set(&format!("/room/{}", uuid));

    let room = api::get_room(uuid, ctx).context("Unknown room")?;
    if room.is_closed() {
        return Err(anyhow::anyhow!("This room is closed, you're late").into());
    }
    let (yaml, _encoding, has_errors) = encoding_rs::UTF_8.decode(&yaml);

    if has_errors {
        return Err(Error(anyhow::anyhow!("Error while decoding the yaml")));
    }

    let reader = BufReader::new(yaml.as_bytes());
    let documents = yaml_split::DocumentIterator::new(reader);
    let documents = documents
        .into_iter()
        .map(|doc| {
            let Ok(doc) = doc else {
                anyhow::bail!("Invalid yaml file")
            };
            let Ok(parsed) = serde_yaml::from_str(&doc) else {
                anyhow::bail!("Invalid yaml file")
            };
            Ok((doc, parsed))
        })
        .collect::<anyhow::Result<Vec<(String, YamlFile)>>>()?;

    let yamls_in_room = api::get_yamls_for_room(uuid, ctx).context("Couldn't get room yamls")?;
    let mut players_in_room = yamls_in_room
        .iter()
        .map(|yaml| yaml.player_name.clone())
        .collect::<HashSet<String>>();

    for (_document, parsed) in documents.iter() {
        if parsed.name.contains("{NUMBER}") || parsed.name.contains("{number}") {
            continue;
        }
        if parsed.name.contains("{PLAYER}") || parsed.name.contains("{player}") {
            continue;
        }

        if players_in_room.contains(&parsed.name) {
            return Err(Error(anyhow::anyhow!(
                "Adding this yaml would duplicate a player name"
            )));
        }
        players_in_room.insert(parsed.name.clone());
    }

    // TODO: Check supported game

    for (document, parsed) in documents {
        api::add_yaml_to_room(uuid, session.user_id, &document, &parsed, ctx).unwrap();
    }

    Ok(Redirect::to(uri!(room(uuid))))
}

#[get("/room/<room_id>/delete/<yaml_id>")]
fn delete_yaml(
    redirect_to: &RedirectTo,
    room_id: Uuid,
    yaml_id: Uuid,
    session: Session,
    ctx: &State<Context>,
) -> Result<Redirect> {
    redirect_to.set(&format!("/room/{}", room_id));

    let room = api::get_room(room_id, ctx).context("Unknown room")?;
    if room.is_closed() {
        return Err(anyhow::anyhow!("This room is closed, you're late").into());
    }

    let yaml = api::get_yaml_by_id(yaml_id, ctx)?;

    if yaml.owner_id.0 != session.user_id && !session.is_admin {
        Err(anyhow::anyhow!("Can't delete a yaml file that isn't yours"))?
    }

    api::remove_yaml(yaml_id, ctx)?;

    Ok(Redirect::to(format!("/room/{}", room_id)))
}

#[derive(rocket::Responder)]
#[response(status = 200, content_type = "application/zip")]
struct ZipFile<'a> {
    content: Vec<u8>,
    headers: Header<'a>,
}

#[get("/room/<room_id>/yamls")]
fn download_yamls<'a>(
    redirect_to: &RedirectTo,
    room_id: Uuid,
    ctx: &State<Context>,
    _session: AdminSession,
) -> Result<ZipFile<'a>> {
    redirect_to.set(&format!("/room/{}", room_id));

    let room = api::get_room(room_id, ctx)?;
    let yamls = api::get_yamls_for_room(room_id, ctx)?;
    let mut writer = zip::ZipWriter::new(Cursor::new(vec![]));

    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for yaml in yamls {
        writer.start_file(&format!("{}.yaml", yaml.id), options)?;
        writer.write_all(yaml.content.as_bytes())?;
    }

    let res = writer.finish()?;
    let value = format!(
        "attachment; filename=\"yamls-{}.zip\"",
        room.close_date.format("%Y-%m-%d_%H_%M_%S")
    );
    Ok(ZipFile {
        content: res.into_inner(),
        headers: Header::new(CONTENT_DISPOSITION.as_str(), value),
    })
}

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations/");

fn run_migrations(
    connection: &mut impl MigrationHarness<Sqlite>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    connection.run_pending_migrations(MIGRATIONS)?;

    Ok(())
}

#[get("/static/<file..>")]
fn dist(file: PathBuf) -> Option<(ContentType, Cow<'static, [u8]>)> {
    let filename = file.display().to_string();
    let asset = Asset::get(&filename)?;
    let content_type = file
        .extension()
        .and_then(OsStr::to_str)
        .and_then(ContentType::from_extension)
        .unwrap_or(ContentType::Bytes);

    Some((content_type, asset.data))
}

#[derive(rust_embed::RustEmbed)]
#[folder = "./static/"]
struct Asset;

#[launch]
fn rocket() -> _ {
    let db_url = std::env::var("DATABASE_URL").expect("Plox provide a DATABASE_URL env variable");
    let _admin_token =
        std::env::var("ADMIN_TOKEN").expect("Plox provide a ADMIN_TOKEN env variable");

    let manager = ConnectionManager::<SqliteConnection>::new(db_url);
    let db_pool = Pool::new(manager).expect("Failed to create database pool, aborting");
    {
        let mut connection = db_pool
            .get()
            .expect("Failed to get database connection to run migrations");
        run_migrations(&mut connection).expect("Failed to run migrations");
    }

    let ctx = Context { db_pool };

    let limits = Limits::default().limit("bytes", 2.megabytes());

    let figment = rocket::Config::figment().merge(("limits", limits));

    rocket::custom(figment)
        .mount(
            "/",
            routes![root, room, upload_yaml, delete_yaml, download_yamls, dist],
        )
        .mount("/admin/", admin::routes())
        .mount("/auth/", auth::routes())
        .manage(ctx)
}
