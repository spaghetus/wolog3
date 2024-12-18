mod db;
mod pandoc;

#[macro_use]
extern crate rocket;

use chrono::NaiveDate;
use clap::Parser;
use db::{Search, SortType};
use pandoc::{ast_to_html, run_postproc_filters};
use rocket::{
    form::{FromFormField, ValueField},
    fs::{FileServer, Options},
    futures::FutureExt,
    http::ContentType,
    response::content::RawHtml,
    State,
};
use serde::Serialize;
use serde_json::json;
use sqlx::{migrate, Pool, Postgres};
use std::{
    ops::{Bound, Deref},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tera::Tera;
use tokio::sync::RwLock;
use url::Url;

#[derive(clap::Parser)]
struct Config {
    #[arg(short, long, default_value = "./articles", env = "WOLOG_CONTENT_ROOT")]
    content_root: PathBuf,
    #[arg(short, long, default_value = "./static", env = "WOLOG_STATIC_ROOT")]
    static_root: PathBuf,
    #[arg(
        short,
        long,
        default_value = "./articles/assets",
        env = "WOLOG_ASSETS_ROOT"
    )]
    assets_root: PathBuf,
    #[arg(
        short,
        long,
        default_value = "./templates/*.html.tera",
        env = "WOLOG_TEMPLATES_ROOT"
    )]
    templates_root: PathBuf,
    #[arg(short, long, default_value = "https://wolo.dev/", env = "WOLOG_ORIGIN")]
    origin: Url,
    #[arg(short = 'W', long)]
    enable_webmention: bool,
    #[arg(short, long, default_value = "false")]
    develop: bool,
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    #[arg(long, default_value = "60")]
    update_interval: u64,
}

#[rocket::main]
async fn main() {
    let config = Arc::new(Config::parse());
    let db = Pool::<Postgres>::connect(&config.database_url)
        .await
        .expect("Connect to database");
    migrate!().run(&db).await.expect("Run migrations");
    let tera = Arc::new(RwLock::new({
        Tera::new(config.templates_root.to_str().unwrap()).expect("Tera failure")
    }));
    tokio::spawn({
        let db = db.clone();
        let cfg = config.clone();
        let tera = tera.clone();
        tokio::time::sleep(Duration::from_millis(100)).await;
        async move {
            let mut periodic = tokio::time::interval(Duration::from_secs(cfg.update_interval));
            loop {
                if let Err(e) = db::update_all(cfg.clone(), &db).await {
                    eprintln!("Error in periodic database update: {e}")
                };
                if let Err(e) = tera.write().await.full_reload() {
                    eprintln!("Error in periodic template reload: {e}")
                };
                periodic.tick().await;
            }
        }
    });
    rocket::build()
        .manage(db)
        .manage(tera.clone())
        .mount(
            "/static",
            FileServer::new(&config.static_root, Options::None),
        )
        .mount(
            "/assets",
            FileServer::new(&config.assets_root, Options::None),
        )
        .mount("/", routes![index, page, tags, search, webmention])
        .launch()
        .await
        .unwrap();
}

#[get("/")]
async fn index(
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
) -> Result<RawHtml<String>, String> {
    page(db, tera, PathBuf::from_str("index").unwrap()).await
}

macro_rules! context {
    ($c:tt) => {
        tera::Context::from_serialize(serde_json::json!($c)).unwrap()
    };
}

#[get("/post/<path..>")]
async fn page(
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
    path: PathBuf,
) -> Result<RawHtml<String>, String> {
    let path = path.to_string_lossy();
    let (ast, meta) = db::read_post(db, &path).await.ok_or("Post not found")?;
    let content = run_postproc_filters(db, tera, ast).await;
    let content = ast_to_html(content)
        .await
        .ok_or("Converting ast to html failed")?;
    let content = tera
        .read()
        .await
        .render(
            &meta.template,
            &context!({
                "toc": meta.toc.iter().map(ToString::to_string).collect::<String>(),
                "meta": &meta,
                "content": &content
            }),
        )
        .expect("Tera rendering failure");
    Ok(RawHtml(content))
}

#[post("/webmention")]
async fn webmention() -> (ContentType, String) {
    todo!()
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct DateField(pub NaiveDate);

impl Deref for DateField {
    type Target = NaiveDate;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'r> FromFormField<'r> for DateField {
    fn from_value(field: ValueField<'r>) -> rocket::form::Result<'r, Self> {
        use rocket::form::error::*;
        let content = field.value;
        if content.is_empty() {
            return Err(Errors::from(ErrorKind::Missing));
        }
        NaiveDate::from_str(content)
            .map(Self)
            .map_err(|e| Errors::from(ErrorKind::Validation(e.to_string().into())))
    }
}

#[derive(Clone, FromForm, Serialize)]
struct SearchForm {
    #[field(default = "")]
    pub search_path: String,
    pub exclude_path: Vec<String>,
    pub tag: Vec<String>,
    pub created_after: Option<DateField>,
    pub created_before: Option<DateField>,
    pub updated_after: Option<DateField>,
    pub updated_before: Option<DateField>,
    pub title_filter: Option<String>,
    #[field(default = SortType::CreateDesc)]
    pub sort_type: SortType,
    pub limit: Option<u32>,
}

impl<'a> From<&'a SearchForm> for Search {
    fn from(value: &'a SearchForm) -> Self {
        Search {
            search_path: value.search_path.clone(),
            exclude_paths: value.exclude_path.clone(),
            tags: value.tag.clone(),
            created: (
                value
                    .created_after
                    .map(|DateField(d)| Bound::Included(d))
                    .unwrap_or(Bound::Unbounded),
                value
                    .created_before
                    .map(|DateField(d)| Bound::Included(d))
                    .unwrap_or(Bound::Unbounded),
            ),
            updated: (
                value
                    .updated_after
                    .map(|DateField(d)| Bound::Included(d))
                    .unwrap_or(Bound::Unbounded),
                value
                    .updated_before
                    .map(|DateField(d)| Bound::Included(d))
                    .unwrap_or(Bound::Unbounded),
            ),
            title_filter: value.title_filter.clone(),
            sort_type: value.sort_type,
            limit: value.limit,
        }
    }
}

#[get("/tags")]
async fn tags(
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
) -> Result<RawHtml<String>, String> {
    let tags = db::tag_counts(db).await.map_err(|e| e.to_string())?;
    let context = context!({
        "tags": tags
    });
    let content = tera
        .read()
        .await
        .render("tag-directory.html.tera", &context)
        .expect("Tera rendering failure");
    Ok(RawHtml(content))
}

#[get("/search?<search_form..>")]
async fn search(
    search_form: SearchForm,
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
) -> Result<RawHtml<String>, String> {
    let search: Search = (&search_form).into();
    let search_url: Url = (&search).into();
    let results = db::search(db, &search).await.map_err(|e| e.to_string())?;
    let tags = db::tags(db).await.map_err(|e| e.to_string())?;
    let context = context!({
        "search": search_form,
        "articles": results,
        "search_qs": search_url.query().unwrap_or(""),
        "tags": tags
    });
    let content = tera
        .read()
        .await
        .render("page-list.html.tera", &context)
        .expect("Tera rendering failure");
    Ok(RawHtml(content))
}