#![warn(clippy::pedantic)]

mod cookies;
mod db;
mod guestbook;
mod oauth;
mod pandoc;

#[macro_use]
extern crate rocket;

use chrono::{NaiveDate, Utc};
use cookies::ClientPersist;
use db::{PostType, Search, SortType};
use figment::{
	providers::{Env, Format, Toml},
	Figment,
};
use notify::{poll, EventKind, Watcher};
use oauth::OAuthProvider;
use pandoc::{ast_to_html, run_postproc_filters};
use reqwest::StatusCode;
use rocket::{
	form::{Form, FromFormField, ValueField},
	fs::{FileServer, Options},
	futures::StreamExt,
	http::{ContentType, CookieJar, Status},
	response::content::RawHtml,
	State,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{migrate, Pool, Postgres};
use std::{
	collections::{HashMap, HashSet},
	ops::{Bound, Deref},
	path::PathBuf,
	str::FromStr,
	sync::Arc,
	time::Duration,
};
use tera::Tera;
use tokio::sync::RwLock;
use url::Url;

#[derive(Serialize, Deserialize, Clone)]
struct Config {
	content_root: PathBuf,
	static_root: PathBuf,
	assets_root: PathBuf,
	templates_root: PathBuf,
	origin: Url,
	#[serde(default)]
	enable_webmention: bool,
	#[serde(default)]
	develop: bool,
	database_url: String,
	update_interval: u64,
	#[serde(default)]
	author: Option<String>,
	#[serde(default)]
	webmention_bans: Vec<String>,
	#[serde(default)]
	guestbook_bans: HashMap<String, HashSet<String>>,
	#[serde(default)]
	oauth_providers: HashMap<String, OAuthProvider>,
}

#[rocket::launch]
async fn launch() -> _ {
	let figment = Figment::new()
		.merge(Toml::file("wolog.toml"))
		.merge(Env::prefixed("WOLOG_").split("__"))
		.join(figment::providers::Serialized::from(
			json!({
				"content_root": "./articles",
				"static_root": "./static",
				"assets_root": "./articles/assets",
				"templates_root": "./templates/*.tera",
				"origin": "https://wolo.dev/",
				"update_interval": 60,
				"database_url": std::env::var("DATABASE_URL").ok(),
			}),
			"default",
		));
	let config: Arc<Config> = Arc::new(figment.extract().expect("Invalid config"));
	dbg!(config.oauth_providers.keys().collect::<Vec<_>>());
	let db = Pool::<Postgres>::connect(&config.database_url)
		.await
		.expect("Connect to database");
	migrate!().run(&db).await.expect("Run migrations");
	let tera = Arc::new(RwLock::new({
		Tera::new(config.templates_root.to_str().unwrap()).expect("Tera failure")
	}));
	tokio::spawn({
		let db = db.clone();
		let config = config.clone();
		async move {
			if let Err(e) = db::update_all(config, &db).await {
				eprintln!("Error in initial database update: {e}");
			}
		}
	});
	setup_watcher(&db, config.clone(), tera.clone());

	rocket::build()
		.manage(db)
		.manage(tera.clone())
		.manage(config.clone())
		.mount(
			"/static",
			FileServer::new(&config.static_root, Options::None),
		)
		.mount(
			"/assets",
			FileServer::new(&config.assets_root, Options::None),
		)
		.mount(
			"/",
			routes![index, page, tags, search, search_feed, webmention],
		)
		.mount(
			"/login",
			routes![oauth::login, oauth::callback, oauth::clear, oauth::forgetme],
		)
		.mount("/guestbook", routes![guestbook::display, guestbook::sign,])
}

fn setup_watcher(db: &Pool<Postgres>, config: Arc<Config>, tera: Arc<RwLock<Tera>>) {
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
	let mut inotify_watcher = Box::new(
		notify::INotifyWatcher::new(
			{
				let tx = tx.clone();
				move |e| {
					if let Ok(e) = e {
						tx.send(e).unwrap();
					}
				}
			},
			notify::Config::default().with_follow_symlinks(true),
		)
		.unwrap(),
	);
	let mut poll_watcher = Box::new(
		notify::PollWatcher::new(
			{
				let tx = tx.clone();
				move |e| {
					if let Ok(e) = e {
						tx.send(e).unwrap();
					}
				}
			},
			notify::Config::default()
				.with_poll_interval(Duration::from_secs(config.update_interval)),
		)
		.unwrap(),
	);
	inotify_watcher
		.watch(
			&config.content_root.canonicalize().unwrap(),
			notify::RecursiveMode::Recursive,
		)
		.unwrap();
	poll_watcher
		.watch(
			&config.content_root.canonicalize().unwrap(),
			notify::RecursiveMode::Recursive,
		)
		.unwrap();
	Box::leak(inotify_watcher);
	Box::leak(poll_watcher);
	tokio::spawn({
		let cfg = config.clone();
		let db = db.clone();
		let root_path = cfg
			.content_root
			.canonicalize()
			.unwrap()
			.to_str()
			.unwrap()
			.to_string();
		async move {
			loop {
				let Some(event) = rx.recv().await else {
					continue;
				};
				if !matches!(
					event.kind,
					EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
				) {
					continue;
				}
				for path in event.paths {
					let Some(path) = path.to_str() else {
						continue;
					};
					let path = path.trim_start_matches(&root_path);
					if path.is_empty() {
						continue;
					}

					if let Err(e) = db::update_one(&cfg, &db, path).await {
						eprintln!("Error in initial database update: {e}");
					};
				}
			}
		}
	});
	tokio::spawn(async move {
		let mut periodic = tokio::time::interval(Duration::from_secs(config.update_interval));
		loop {
			if let Err(e) = tera.write().await.full_reload() {
				eprintln!("Error in periodic template reload: {e}");
			}
			periodic.tick().await;
		}
	});
}

#[get("/")]
async fn index(
	db: &State<Pool<Postgres>>,
	tera: &State<Arc<RwLock<Tera>>>,
	cookie: ClientPersist,
	jar: &CookieJar<'_>,
	config: &State<Arc<Config>>,
) -> Result<RawHtml<String>, String> {
	page(
		db,
		tera,
		cookie,
		jar,
		PathBuf::from_str("index").unwrap(),
		false,
		config,
	)
	.await
}

#[macro_export]
macro_rules! context {
	($c:tt) => {
		tera::Context::from_serialize(serde_json::json!($c)).unwrap()
	};
}

#[get("/post/<path..>?<bare>")]
async fn page(
	db: &State<Pool<Postgres>>,
	tera: &State<Arc<RwLock<Tera>>>,
	mut cookie: ClientPersist,
	jar: &CookieJar<'_>,
	path: PathBuf,
	bare: bool,
	config: &State<Arc<Config>>,
) -> Result<RawHtml<String>, String> {
	let path = &db::trim_path(&path);
	let (ast, meta) = db::read_post(db, path).await.ok_or("Post not found")?;
	let content = run_postproc_filters(db, tera, ast, path, &cookie, config).await;
	let content = ast_to_html(content)
		.await
		.ok_or("Converting ast to html failed")?;
	if bare {
		return Ok(RawHtml(content));
	}
	let mentioners = db::mentioners(db, path).await.unwrap_or_default();
	let guestbook_size = db::guestbook_size(db, path).await.unwrap_or(0);
	let content = tera
		.read()
		.await
		.render(
			format!("{}.html.tera", meta.template).as_str(),
			&context!({
				"path": path,
				"toc": meta.toc.iter().map(ToString::to_string).collect::<String>(),
				"meta": &meta,
				"cookie": &cookie,
				"mentioners": &mentioners,
				"content": &content,
				"guestbook_size": guestbook_size,
				"has_oauth": !config.oauth_providers.is_empty()
			}),
		)
		.expect("Tera rendering failure");
	cookie
		.viewed
		.insert(path.to_string(), Utc::now().date_naive());
	jar.add(cookie);
	Ok(RawHtml(content))
}

#[derive(FromForm)]
struct WebMention {
	pub source: String,
	pub target: String,
}

#[post("/webmention", data = "<wm>")]
async fn webmention(
	wm: Form<WebMention>,
	cfg: &State<Arc<Config>>,
	db: &State<Pool<Postgres>>,
) -> Result<&'static str, (Status, String)> {
	use reqwest::Url;
	let WebMention { source, target } = wm.into_inner();
	let source =
		Url::parse(&source).map_err(|_| (Status::BadRequest, "Bad source URL".to_string()))?;
	if source.cannot_be_a_base() {
		return Err((
			Status::BadRequest,
			"Source URL should be absolute".to_string(),
		));
	}
	let target =
		Url::parse(&target).map_err(|_| (Status::BadRequest, "Bad target URL".to_string()))?;
	if target.cannot_be_a_base() {
		return Err((
			Status::BadRequest,
			"Target URL should be absolute".to_string(),
		));
	}
	if target.host() != cfg.origin.host() {
		return Err((
			Status::BadRequest,
			"Target URL should be one of our pages".to_string(),
		));
	}
	let Some(path) = target
		.path()
		.trim_end_matches('/')
		.trim_end_matches(".md")
		.get("/post/".len()..)
	else {
		return Err((Status::BadRequest, "You can only mention posts".to_string()));
	};

	let client = reqwest::ClientBuilder::new()
		.timeout(Duration::from_millis(300))
		.build()
		.unwrap();
	let Ok(response) = client.get(source.clone()).send().await else {
		return Err((
			Status::BadRequest,
			"Couldn't fetch the mentioning page".to_string(),
		));
	};
	if response.status() == StatusCode::NOT_FOUND || response.status() == StatusCode::GONE {
		let _ = db::rm_webmention(db, &source, path).await;
		return Err((
            Status::BadRequest,
            "The mentioning page is missing? (if we had it before, we've done our best to delete it)".to_string(),
        ));
	}
	let Ok(response) = response.text().await else {
		return Err((
			Status::BadRequest,
			"The mentioning page doesn't seem to be UTF-8?".to_string(),
		));
	};
	if !response.contains(target.as_str()) {
		let _ = db::rm_webmention(db, &source, path).await;
		return Err((
            Status::BadRequest,
            "The page doesn't mention us like you said it would? (if we had it before, we've deleted it)".to_string(),
        ));
	}
	db::get_webmention(db, &source, path)
		.await
		.map_err(|_| (Status::InternalServerError, "Database error".to_string()))?;
	Ok("Looks OK! The webmention should be registered now.")
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
		use rocket::form::error::{ErrorKind, Errors};
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
	pub post_type: Option<PostType>,
	pub limit: Option<u32>,
}

impl<'a> From<&'a SearchForm> for Search {
	fn from(value: &'a SearchForm) -> Self {
		Search {
			search_path: value.search_path.clone(),
			exclude_paths: value.exclude_path.clone(),
			tags: value.tag.clone(),
			post_type: value.post_type,
			created: (
				value
					.created_after
					.map_or(Bound::Unbounded, |DateField(d)| Bound::Included(d)),
				value
					.created_before
					.map_or(Bound::Unbounded, |DateField(d)| Bound::Included(d)),
			),
			updated: (
				value
					.updated_after
					.map_or(Bound::Unbounded, |DateField(d)| Bound::Included(d)),
				value
					.updated_before
					.map_or(Bound::Unbounded, |DateField(d)| Bound::Included(d)),
			),
			title_filter: value.title_filter.clone(),
			sort_type: value.sort_type,
			limit: value.limit,
			ignore_hidden: false,
			extra: tera::Value::Object(Default::default()),
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
	cookie: ClientPersist,
	tera: &State<Arc<RwLock<Tera>>>,
) -> Result<RawHtml<String>, (Status, String)> {
	let search: Search = (&search_form).into();
	let search_url: Url = (&search).into();
	let results = db::search(db, &search)
		.await
		.map_err(|e| (Status::InternalServerError, e.to_string()))?;
	let tags = db::tags(db)
		.await
		.map_err(|e| (Status::InternalServerError, e.to_string()))?;
	let context = context!({
		"search": search_form,
		"articles": results,
		"cookie": &cookie,
		"new": results.iter().filter(|(path, meta)| {
			let Some(viewed) = cookie.viewed.get(path) else {return true};
			*viewed < meta.updated
		}).map(|(p, _)| p).collect::<HashSet<_>>(),
		"search_qs": search_url.query().unwrap_or(""),
		"tags": tags
	});
	let content = tera
		.read()
		.await
		.render("page-list.html.tera", &context)
		.map_err(|e| {
			dbg!(&e);
			(
				Status::InternalServerError,
				format!("I couldn't finalize rendering this page because: {e}"),
			)
		})?;
	Ok(RawHtml(content))
}

#[get("/feed?<search_form..>")]
async fn search_feed(
	search_form: SearchForm,
	db: &State<Pool<Postgres>>,
	tera: &State<Arc<RwLock<Tera>>>,
	config: &State<Arc<Config>>,
) -> Result<(ContentType, String), (Status, String)> {
	let mut search: Search = (&search_form).into();
	search.limit = search.limit.map_or(32, |l| l.min(32)).into();
	search.sort_type = SortType::CreateDesc;
	let search_url: Url = (&search).into();
	let search_qs = search_url.query().unwrap_or("");
	search_atom_inner(
		search,
		db,
		tera,
		"Wolog (Search)".to_string().into(),
		"/favicon.ico".to_string().into(),
		"/banner.png".to_string().into(),
		config,
		format!("{}feed?{search_qs}", config.origin),
	)
	.await
}

#[allow(clippy::too_many_arguments)]
async fn search_atom_inner(
	search: Search,
	db: &State<Pool<Postgres>>,
	tera: &State<Arc<RwLock<Tera>>>,
	title: Option<String>,
	icon: Option<String>,
	logo: Option<String>,
	config: &State<Arc<Config>>,
	feed_url: String,
) -> Result<(ContentType, String), (Status, String)> {
	let results = db::search(db, &search)
		.await
		.map_err(|e| (Status::InternalServerError, e.to_string()))?;
	let context = context!({
		"articles": results,
		"url": feed_url,
		"title": title,
		"icon": icon,
		"logo": logo,
		"config": (*config).clone(),
	});
	let content = tera
		.read()
		.await
		.render("page-list.atom.tera", &context)
		.map_err(|e| {
			dbg!(&e);
			(
				Status::InternalServerError,
				format!("I couldn't finalize rendering this page because: {e}"),
			)
		})?;
	Ok((ContentType::new("application", "atom+xml"), content))
}
