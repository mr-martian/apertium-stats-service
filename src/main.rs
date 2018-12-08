#![feature(custom_attribute, try_trait, custom_derive, proc_macro_hygiene, decl_macro)]
#![deny(clippy::all)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::suspicious_else_formatting)]
#![allow(clippy::print_literal)]
#![allow(proc_macro_derive_resolution_fallback)]

mod db;
mod models;
mod schema;
mod stats;
mod util;
mod worker;

#[cfg(test)]
#[macro_use]
mod tests;

extern crate chrono;
#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_derive_enum;
extern crate dotenv;
#[macro_use]
extern crate lazy_static;
extern crate regex;
#[macro_use]
extern crate rocket;
extern crate tempfile;
#[macro_use]
extern crate rocket_contrib;
extern crate rocket_cors;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate tokio;
extern crate tokio_process;
#[macro_use]
extern crate slog;
extern crate hyper;
extern crate hyper_tls;
extern crate quick_xml;
extern crate slog_async;
extern crate slog_term;

use std::env;

use diesel::{dsl::sql, prelude::*};
use dotenv::dotenv;
use hyper::{client::connect::HttpConnector, Client};
use hyper_tls::HttpsConnector;
use rocket::{
    http::{Accept, ContentType, MediaType, Method, Status},
    request::Form,
    response::Content,
    State,
};
use rocket_contrib::json::JsonValue;
use rocket_cors::{AllowedHeaders, AllowedOrigins};
use slog::{Drain, Logger};
use tokio::{executor::current_thread::CurrentThread, prelude::Future, runtime::Runtime};

use db::DbConn;
use models::FileKind;
use schema::entries as entries_db;
use util::{normalize_name, JsonResult, Params};
use worker::{Task, Worker};

pub const ORGANIZATION_ROOT: &str = "https://github.com/apertium";
pub const ORGANIZATION_RAW_ROOT: &str = "https://raw.githubusercontent.com/apertium";
pub const LANG_CODE_RE: &str = r"\w{2,3}(_\w+)?";

lazy_static! {
    pub static ref RUNTIME: Runtime = Runtime::new().unwrap();
    pub static ref HTTPS_CLIENT: Client<HttpsConnector<HttpConnector>> = Client::builder()
        .executor(RUNTIME.executor())
        .build(HttpsConnector::new(4).unwrap());
}

fn launch_tasks_and_reply(
    worker: &State<Worker>,
    name: String,
    kind: Option<&FileKind>,
    options: Params,
) -> JsonResult {
    match worker.launch_tasks(&HTTPS_CLIENT, &name, kind, options.is_recursive()) {
        Ok((ref new_tasks, ref in_progress_tasks, ref _future))
            if new_tasks.is_empty() && in_progress_tasks.is_empty() =>
        {
            JsonResult::Err(
                Some(json!({
                    "name": name,
                    "error": "No recognized files",
                })),
                Status::NotFound,
            )
        },
        Ok((_new_tasks, in_progress_tasks, future)) => {
            if options.is_async() {
                let detached_future = future.map(|_| ()).map_err(|_| ());
                RUNTIME.executor().spawn(detached_future);

                JsonResult::Err(
                    Some(json!({
                        "name": name,
                        "in_progress": in_progress_tasks,
                    })),
                    Status::Accepted,
                )
            } else {
                match CurrentThread::new().block_on(future) {
                    Ok(stats) => JsonResult::Ok(json!({
                        "name": name,
                        "stats": stats,
                        "in_progress": vec![] as Vec<Task>,
                    })),
                    Err(_err) => JsonResult::Err(None, Status::InternalServerError),
                }
            }
        },
        Err(error) => JsonResult::Err(
            Some(json!({
                "name": name,
                "error": error,
            })),
            Status::BadRequest,
        ),
    }
}

fn parse_name_param(name: &str) -> Result<String, (Option<JsonValue>, Status)> {
    normalize_name(name).map_err(|err| {
        (
            Some(json!({
                "name": name,
                "error": err,
            })),
            Status::BadRequest,
        )
    })
}

fn parse_kind_param(name: &str, kind: &str) -> Result<FileKind, (Option<JsonValue>, Status)> {
    FileKind::from_string(kind).map_err(|err| {
        (
            Some(json!({
                "name": name,
                "error": err,
            })),
            Status::BadRequest,
        )
    })
}

#[get("/")]
fn index<'a>(accept: Option<&'a Accept>) -> Content<&'a str> {
    if accept.map_or(false, |a| a.preferred().media_type() == &MediaType::HTML) {
        Content(ContentType::HTML, include_str!("../index.html"))
    } else {
        Content(
            ContentType::Plain,
            "USAGE

GET /apertium-<code1>(-<code2>)
retrieves statistics for the specified package

GET /apertium-<code1>(-<code2>)/<kind>
retrieves <kind> statistics for the specified package

POST /apertium-<code1>(-<code2>)
calculates statistics for the specified package

POST /apertium-<code1>(-<code2>)/<kind>
calculates <kind> statistics for the specified package

See /openapi.yaml for full specification.",
        )
    }
}

#[get("/openapi.yaml")]
fn openapi_yaml() -> Content<&'static str> {
    Content(
        ContentType::new("application", "x-yaml"),
        include_str!("../openapi.yaml"),
    )
}

#[get("/<name>?<params..>")]
fn get_stats(name: String, params: Form<Option<Params>>, conn: DbConn, worker: State<Worker>) -> JsonResult {
    let name = parse_name_param(&name)?;

    let entries: Vec<models::Entry> = entries_db::table
        .filter(entries_db::name.eq(&name))
        .order(entries_db::created)
        .limit(1)
        .load::<models::Entry>(&*conn)
        .map_err(|_| (None, Status::InternalServerError))?;

    if entries.is_empty() {
        if let Some(in_progress_tasks) = worker.get_tasks_in_progress(&name) {
            JsonResult::Err(
                Some(json!({
                    "name": name,
                    "in_progress": in_progress_tasks,
                })),
                Status::TooManyRequests,
            )
        } else {
            drop(conn);
            launch_tasks_and_reply(&worker, name, None, params.into_inner().unwrap_or_default())
        }
    } else {
        let entries = entries_db::table
            .filter(entries_db::name.eq(&name))
            .filter(sql("1 GROUP BY stat_kind, path")) // HACK: Diesel has no real group_by :(
            .order(entries_db::created)
            .load::<models::Entry>(&*conn)
            .map_err(|_| (None, Status::InternalServerError))?;
        JsonResult::Ok(json!({
            "name": name,
            "stats": entries,
            "in_progress": worker.get_tasks_in_progress(&name).unwrap_or_else(|| vec![]),
        }))
    }
}

#[get("/<name>/<kind>?<params..>")]
fn get_specific_stats(
    name: String,
    kind: String,
    params: Form<Option<Params>>,
    conn: DbConn,
    worker: State<Worker>,
) -> JsonResult {
    let name = parse_name_param(&name)?;
    let file_kind = parse_kind_param(&name, &kind)?;

    let entries: Vec<models::Entry> = entries_db::table
        .filter(entries_db::name.eq(&name))
        .filter(entries_db::file_kind.eq(&file_kind))
        .order(entries_db::created)
        .limit(1)
        .load::<models::Entry>(&*conn)
        .map_err(|_| (None, Status::InternalServerError))?;

    if entries.is_empty() {
        if let Some(in_progress_tasks) = worker.get_tasks_in_progress(&name) {
            if in_progress_tasks.iter().filter(|task| task.kind == file_kind).count() != 0 {
                return JsonResult::Err(
                    Some(json!({
                        "name": name,
                        "in_progress": in_progress_tasks,
                    })),
                    Status::TooManyRequests,
                );
            }
        }

        drop(conn);
        launch_tasks_and_reply(&worker, name, Some(&file_kind), params.into_inner().unwrap_or_default())
    } else {
        let entries = entries_db::table
            .filter(entries_db::name.eq(&name))
            .filter(entries_db::file_kind.eq(&file_kind))
            .filter(sql("1 GROUP BY stat_kind, path")) // HACK: Diesel has no real group_by :(
            .order(entries_db::created)
            .load::<models::Entry>(&*conn)
            .map_err(|_| (None, Status::InternalServerError))?;
        JsonResult::Ok(json!({
            "name": name,
            "stats": entries,
            "in_progress": worker.get_tasks_in_progress(&name).unwrap_or_else(|| vec![]),
        }))
    }
}

#[post("/<name>?<params..>")]
fn calculate_stats(name: String, params: Form<Option<Params>>, worker: State<Worker>) -> JsonResult {
    let name = parse_name_param(&name)?;
    launch_tasks_and_reply(&worker, name, None, params.into_inner().unwrap_or_default())
}

#[post("/<name>/<kind>?<params..>")]
fn calculate_specific_stats(
    name: String,
    kind: String,
    params: Form<Option<Params>>,
    worker: State<Worker>,
) -> JsonResult {
    let name = parse_name_param(&name)?;
    let file_kind = parse_kind_param(&name, &kind)?;
    launch_tasks_and_reply(&worker, name, Some(&file_kind), params.into_inner().unwrap_or_default())
}

fn create_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let async_drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(async_drain, o!())
}

fn rocket(database_url: String) -> rocket::Rocket {
    let pool = db::init_pool(&database_url);
    let logger = create_logger();
    let worker = Worker::new(pool.clone(), logger.clone());

    let cors_options = rocket_cors::Cors {
        allowed_origins: AllowedOrigins::all(),
        allowed_methods: vec![Method::Get, Method::Post].into_iter().map(From::from).collect(),
        allowed_headers: AllowedHeaders::some(&["Authorization", "Accept"]),
        allow_credentials: true,
        ..Default::default()
    };

    rocket::ignite()
        .manage(pool)
        .manage(worker)
        .manage(logger)
        .mount(
            "/",
            routes![
                index,
                openapi_yaml,
                get_stats,
                get_specific_stats,
                calculate_stats,
                calculate_specific_stats,
            ],
        )
        .attach(cors_options)
}

#[cfg_attr(tarpaulin, skip)]
fn main() {
    dotenv().ok();
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    rocket(database_url).launch();
}
