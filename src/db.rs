use crate::{oauth::Identity, pandoc, Config};
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use dom_query::Document;
use pandoc_ast::{Block, Inline, MetaValue, Pandoc};
use reqwest::Client;
use rocket::tokio::{self};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{query, query_as, types::Json, Pool, Postgres};
use std::{
	collections::{BTreeSet, HashMap, HashSet},
	fmt::Display,
	ops::{Bound, RangeBounds},
	str::FromStr,
	sync::Arc,
	time::SystemTime,
};
use strum::EnumString;
use url::Url;
use walkdir::WalkDir;

const DEFAULT_TITLE: &dyn Fn() -> String = &|| "Untitled Page".to_string();
const DEFAULT_TEMPLATE: &dyn Fn() -> String = &|| "article.html.tera".to_string();

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ArticleMeta {
	#[serde(default = "DEFAULT_TITLE")]
	pub title: String,
	#[serde(default)]
	pub post_type: PostType,
	#[serde(default)]
	pub blurb: String,
	#[serde(default)]
	pub tags: Vec<String>,
	#[serde(default = "DEFAULT_TEMPLATE")]
	pub template: String,
	#[serde(default)]
	pub toc: Vec<Toc>,
	#[serde(default)]
	pub exclude_from_rss: bool,
	#[serde(default)]
	pub hidden: bool,
	pub updated: NaiveDate,
	pub created: NaiveDate,
	#[serde(default)]
	pub ready: bool,
	#[serde(default)]
	pub always_rerender: bool,
	#[serde(flatten)]
	pub extra: Value,
	#[serde(default)]
	pub mentioners: Vec<String>,
	#[serde(default)]
	pub mentions: Vec<String>,
}

impl TryFrom<&Pandoc> for ArticleMeta {
	type Error = serde_json::Error;

	fn try_from(pandoc_ast: &Pandoc) -> Result<Self, Self::Error> {
		fn pandoc_inline_to_string(i: &Inline) -> &str {
			match i {
				pandoc_ast::Inline::Str(s) => s.as_str(),
				pandoc_ast::Inline::Space => " ",
				pandoc_ast::Inline::SoftBreak => "\n",
				pandoc_ast::Inline::LineBreak => "\n",
				_ => "",
			}
		}
		fn pandoc_block_to_string(b: &Block) -> String {
			match b {
				Block::Para(i) | Block::Plain(i) => i.iter().map(pandoc_inline_to_string).collect(),
				Block::LineBlock(l) => l
					.iter()
					.map(|l| l.iter().map(pandoc_inline_to_string).collect::<String>() + "\n")
					.collect(),
				Block::RawBlock(_, s) => s.clone(),
				Block::BlockQuote(b) => {
					b.iter().map(|b| pandoc_block_to_string(b) + "\n").collect()
				}
				_ => String::new(),
			}
		}
		fn pandoc_meta_to_value(meta: MetaValue) -> serde_json::Value {
			use serde_json::Value;
			match meta {
				MetaValue::MetaMap(map) => Value::Object(
					map.into_iter()
						.map(|(key, value)| (key, pandoc_meta_to_value(*value)))
						.collect(),
				),
				MetaValue::MetaList(list) => {
					Value::Array(list.into_iter().map(pandoc_meta_to_value).collect())
				}
				MetaValue::MetaBool(b) => Value::Bool(b),
				MetaValue::MetaString(s) => Value::String(s),
				MetaValue::MetaInlines(i) => {
					Value::String(i.iter().map(pandoc_inline_to_string).collect())
				}
				MetaValue::MetaBlocks(b) => {
					Value::String(b.iter().map(pandoc_block_to_string).collect())
				}
			}
		}
		let meta = pandoc_ast
			.meta
			.iter()
			.map(|(key, value)| (key.to_string(), pandoc_meta_to_value(value.clone())))
			.collect();
		let meta = serde_json::Value::Object(meta);
		let meta: ArticleMeta = serde_json::from_value(meta)?;
		Ok(meta)
	}
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Toc {
	Text(String),
	Heading {
		label: String,
		anchor: String,
		subheadings: Vec<Toc>,
	},
}

impl Display for Toc {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Toc::Text(text) => write!(f, "<li>{text}</li>"),
			Toc::Heading {
				label,
				anchor,
				subheadings,
			} if subheadings.is_empty() => {
				write!(f, "<li><a href=\"#{anchor}\">{label}</a></li>")
			}
			Toc::Heading {
				label,
				anchor,
				subheadings,
			} => write!(
				f,
				"<li><a href=\"#{anchor}\">{label}</a><ul>{}</ul></li>",
				subheadings
					.iter()
					.map(std::string::ToString::to_string)
					.collect::<String>()
			),
		}
	}
}

#[derive(
	Serialize,
	Deserialize,
	Clone,
	Copy,
	Debug,
	Default,
	strum::Display,
	FromFormField,
	PartialEq,
	Eq,
)]
pub enum PostType {
	#[default]
	Note,
	Article,
	Like,
	Repost,
	Reply,
}

pub async fn update_all(cfg: Arc<Config>, db: &Pool<Postgres>) -> sqlx::Result<()> {
	update_posts(db, cfg).await?;
	Ok(())
}

async fn update_posts(db: &Pool<Postgres>, cfg: Arc<Config>) -> Result<(), sqlx::Error> {
	let posts: Arc<_> =
		query!(r#"SELECT path, updated as "updated: chrono::DateTime<Utc>", meta FROM posts"#)
			.fetch_all(db)
			.await?
			.into_iter()
			.map(|r| {
				(
					r.path,
					(
						r.updated,
						serde_json::from_value::<ArticleMeta>(r.meta).ok(),
					),
				)
			})
			.collect::<HashMap<_, _>>()
			.into();
	let root: Arc<str> = cfg.content_root.to_str().unwrap().into();
	let (files, all_files): (HashMap<_, _>, HashSet<_>) = tokio::task::spawn_blocking({
		let posts = posts.clone();
		let cfg = cfg.clone();
		let root = root.clone();
		move || {
			let mut all_files = HashSet::new();
			(
				WalkDir::new(&cfg.content_root)
					.into_iter()
					.flatten()
					.filter(|f| {
						!f.file_type().is_dir()
							&& f.path()
								.extension()
								.and_then(|x| x.to_str())
								.is_some_and(|x| x == "md")
					})
					.filter_map(|f| {
						let modified = std::fs::metadata(f.path())
							.and_then(|f| f.modified())
							.unwrap_or_else(|_| SystemTime::now());
						f.path().to_str().map(|f| {
							(
								f.to_string(),
								(
									f.trim_start_matches(&*root)
										.trim_start_matches(['.', '/'])
										.trim_end_matches(".md")
										.to_string(),
									DateTime::<Utc>::from(modified) - Duration::seconds(1),
								),
							)
						})
					})
					.inspect(|(_path, (trimmed, _))| {
						all_files.insert(trimmed.clone());
					})
					.filter(|(_path, (trimmed, updated))| {
						posts
							.get(trimmed)
							.is_none_or(|(cached, _)| updated > cached)
					})
					.collect(),
				all_files,
			)
		}
	})
	.await
	.expect("Failed to search for files");
	let mut group = tokio::task::JoinSet::<Option<()>>::new();
	for (path, (trimmed, updated)) in files {
		let db = db.clone();
		let cfg = cfg.clone();
		let posts = posts.clone();
		group.spawn(async move {
			let ast = pandoc::md_to_ast(&path).await?;
			let ast = pandoc::run_preproc_filters(&db, ast, &trimmed).await;
			let meta = ArticleMeta::try_from(&ast)
				.inspect_err(|e| eprintln!("Failed to load meta from {path} with {e}"))
				.ok()?;
			let old_meta = posts.get(&trimmed).and_then(|(_, meta)| meta.as_ref());
			if !meta.ready && !cfg.develop {
				eprintln!("Skip {path} since it's not ready and we're in production");
				return Some(());
			}
			let ast = serde_json::to_value(&ast).ok()?;
			let meta_json = serde_json::to_value(&meta).ok()?;
			// let path = path
			//     .trim_start_matches(&*root)
			//     .trim_start_matches(['.', '/'])
			//     .trim_end_matches(".md")
			//     .to_string();
			query!(
				"INSERT INTO posts
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT(path) DO UPDATE
                        SET updated = excluded.updated,
                            ast = excluded.ast,
                            meta = excluded.meta",
				trimmed,
				updated,
				ast,
				meta_json
			)
			.execute(&db)
			.await
			.ok()?;
			for to_url in meta
				.mentions
				.iter()
				.chain(old_meta.iter().flat_map(|m| m.mentions.iter()))
			{
				tokio::spawn(send_webmention(
					db.clone(),
					cfg.clone(),
					path.clone(),
					to_url.clone(),
				));
			}
			Some(())
		});
	}
	#[allow(clippy::unnecessary_to_owned)]
	for path in posts
		.keys()
		.filter(|p| !all_files.contains(p.as_str()))
		.cloned()
	{
		let db = db.clone();
		group.spawn(async move {
			query!("DELETE FROM posts WHERE path = $1", path)
				.execute(&db)
				.await
				.ok()?;
			Some(())
		});
	}
	group.join_all().await;
	Ok(())
}

pub async fn read_post(db: &Pool<Postgres>, path: &str) -> Option<(Pandoc, ArticleMeta)> {
	let path = path.trim_start_matches(['.', '/']).trim_end_matches(".md");
	let result = query!(r#"SELECT ast, meta FROM posts WHERE path = $1"#, path)
		.fetch_optional(db)
		.await
		.ok()??;
	// If either of these fail, the schema has probably changed and we need to throw everything out
	let Ok(ast) = serde_json::from_value(result.ast) else {
		query!("DELETE FROM posts").execute(db).await.ok()?;
		return None;
	};
	let Ok(meta) = serde_json::from_value(result.meta) else {
		query!("DELETE FROM posts").execute(db).await.ok()?;
		return None;
	};
	Some((ast, meta))
}

pub async fn read_post_meta(db: &Pool<Postgres>, path: &str) -> Option<ArticleMeta> {
	let path = path.trim_start_matches(['.', '/']).trim_end_matches(".md");
	let result = query!(r#"SELECT meta FROM posts WHERE path = $1"#, path)
		.fetch_optional(db)
		.await
		.ok()??;
	// If either of these fail, the schema has probably changed and we need to throw everything out
	let Ok(meta) = serde_json::from_value(result.meta) else {
		query!("DELETE FROM posts").execute(db).await.ok()?;
		return None;
	};
	Some(meta)
}

#[cfg(not(debug_assertions))]
async fn send_webmention(
	_db: Pool<Postgres>,
	_cfg: Arc<Config>,
	_from_path: String,
	_to_url: String,
) -> Result<bool, String> {
	// This is broken
	Ok(false)
}

#[cfg(debug_assertions)]
async fn send_webmention(
	_db: Pool<Postgres>,
	cfg: Arc<Config>,
	from_path: String,
	to_url: String,
) -> Result<bool, String> {
	tokio::time::sleep(Duration::seconds(30).to_std().unwrap()).await;
	if !cfg.enable_webmention {
		return Ok(false);
	}
	// let post = query!(
	// 	"SELECT updated FROM visible_posts WHERE path = $1",
	// 	from_path
	// )
	// .fetch_optional(&db)
	// .await
	// .map_err(|e| e.to_string())?;
	let mut to_url = Url::from_str(&to_url).map_err(|e| e.to_string())?;
	if !to_url.has_host() {
		to_url.set_host(cfg.origin.host_str()).unwrap();
		to_url.set_scheme(cfg.origin.scheme()).unwrap();
	}
	let Some(response) = reqwest::get(to_url.as_str()).await.ok() else {
		return Ok(false);
	};
	let Some(response) = response.text().await.ok() else {
		return Ok(false);
	};
	let wm_url = {
		let doc = Document::from(response);
		let element = doc.select_single("link[rel=webmention], a[rel=webmention]");
		let wm_url = element.attr("href");
		let Some(wm_url) = wm_url.as_deref() else {
			return Ok(false);
		};
		let Ok(wm_url) = to_url.join(wm_url) else {
			return Ok(false);
		};
		wm_url
	};
	let mut from_url = cfg.origin.clone();
	let mut callback_url = cfg.origin.clone();
	from_url.set_path(&from_path);
	callback_url.set_path("/webmention_callback");
	let client = Client::new();

	let mut form = HashMap::new();
	form.insert("source", from_url.as_str());
	form.insert("target", to_url.as_str());
	client
		.post(wm_url)
		.header("Content-Type", "application/x-www-form-urlencoded")
		.form(&form)
		.send()
		.await
		.map_err(|e| e.to_string())?;
	Ok(true)
}

pub async fn get_webmention(
	db: &Pool<Postgres>,
	source: &Url,
	path: &str,
) -> Result<(), sqlx::Error> {
	let now = Utc::now();
	query!(
		"INSERT INTO incoming_mentions
            VALUES ($1, $2, $3, $3)
            ON CONFLICT (to_path, from_url)
            DO UPDATE
            SET last_mentioned = $3",
		path,
		source.as_str(),
		now
	)
	.execute(db)
	.await?;
	Ok(())
}

pub async fn rm_webmention(
	db: &Pool<Postgres>,
	source: &Url,
	path: &str,
) -> Result<(), sqlx::Error> {
	// let _now = Utc::now();
	query!(
		"DELETE FROM incoming_mentions
            WHERE from_url = $1
            AND to_path = $2",
		source.as_str(),
		path
	)
	.execute(db)
	.await?;
	Ok(())
}

#[derive(Debug, Serialize)]
pub struct Mention {
	pub from_url: String,
	pub last_mentioned: DateTime<Utc>,
	pub first_mentioned: Option<DateTime<Utc>>,
}

pub async fn mentioners(db: &Pool<Postgres>, path: &str) -> Result<Vec<Mention>, sqlx::Error> {
	query_as!(
        Mention,
        "SELECT from_url, last_mentioned, first_mentioned FROM incoming_mentions WHERE to_path = $1",
        path
    )
    .fetch_all(db)
    .await
}

pub async fn tags(db: &Pool<Postgres>) -> Result<BTreeSet<String>, sqlx::Error> {
	let results =
		query!(r#"SELECT (meta->'tags') as "tags: Json<Vec<String>>" FROM visible_posts"#)
			.fetch_all(db)
			.await?;
	Ok(results
		.into_iter()
		.filter_map(|r| r.tags)
		.flat_map(|t| t.0.into_iter())
		.collect())
}

pub async fn tag_counts(db: &Pool<Postgres>) -> Result<HashMap<String, usize>, sqlx::Error> {
	let results =
		query!(r#"SELECT (meta->'tags') as "tags: Json<Vec<String>>" FROM visible_posts"#)
			.fetch_all(db)
			.await?;
	Ok(results
		.into_iter()
		.filter_map(|r| r.tags)
		.flat_map(|t| t.0.into_iter())
		.fold(HashMap::new(), |mut acc, tag| {
			*acc.entry(tag).or_insert(0) += 1;
			acc
		}))
}

pub type Bounds<B> = (Bound<B>, Bound<B>);

fn unbounded<B>() -> Bounds<B> {
	(Bound::Unbounded, Bound::Unbounded)
}

#[derive(
	Serialize, Deserialize, Default, Clone, Copy, Debug, EnumString, strum::Display, FromFormField,
)]
pub enum SortType {
	CreateAsc,
	#[default]
	CreateDesc,
	UpdateAsc,
	UpdateDesc,
	NameAsc,
	NameDesc,
}

pub type Sorter = dyn Fn(&(String, ArticleMeta), &(String, ArticleMeta)) -> std::cmp::Ordering;

impl SortType {
	pub fn sort_fn(&self) -> &Sorter {
		match self {
			SortType::CreateAsc => &|(_, l), (_, r)| l.created.cmp(&r.created),
			SortType::CreateDesc => &|(_, l), (_, r)| r.created.cmp(&l.created),
			SortType::UpdateAsc => &|(_, l), (_, r)| l.updated.cmp(&r.updated),
			SortType::UpdateDesc => &|(_, l), (_, r)| r.updated.cmp(&l.updated),
			SortType::NameAsc => &|(_, l), (_, r)| l.title.cmp(&r.title),
			SortType::NameDesc => &|(_, l), (_, r)| r.title.cmp(&l.title),
		}
	}
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Search {
	#[serde(default)]
	pub search_path: String,
	#[serde(default)]
	pub exclude_paths: Vec<String>,
	#[serde(default)]
	pub tags: Vec<String>,
	#[serde(default)]
	pub post_type: Option<PostType>,
	#[serde(default = "unbounded")]
	pub created: Bounds<NaiveDate>,
	#[serde(default = "unbounded")]
	pub updated: Bounds<NaiveDate>,
	#[serde(default)]
	pub title_filter: Option<String>,
	#[serde(default)]
	pub sort_type: SortType,
	#[serde(default)]
	pub limit: Option<u32>,
	#[serde(default)]
	pub ignore_hidden: bool,
	#[serde(flatten)]
	pub extra: Value,
}

impl<'a> From<&'a Search> for Url {
	fn from(val: &'a Search) -> Self {
		let params = [("sort_type", val.sort_type.to_string())]
			.into_iter()
			.chain(
				(!val.search_path.is_empty()).then_some(("search_path", val.search_path.clone())),
			)
			.chain(
				val.exclude_paths
					.iter()
					.map(|p| ("exclude_path", p.clone())),
			)
			.chain(val.tags.iter().map(|p| ("tag", p.clone())))
			.chain(match val.created.0 {
				Bound::Unbounded => None,
				Bound::Included(d) | Bound::Excluded(d) => Some(("created_after", d.to_string())),
			})
			.chain(match val.created.1 {
				Bound::Unbounded => None,
				Bound::Included(d) | Bound::Excluded(d) => Some(("created_before", d.to_string())),
			})
			.chain(match val.updated.0 {
				Bound::Unbounded => None,
				Bound::Included(d) | Bound::Excluded(d) => Some(("updated_after", d.to_string())),
			})
			.chain(match val.updated.1 {
				Bound::Unbounded => None,
				Bound::Included(d) | Bound::Excluded(d) => Some(("updated_before", d.to_string())),
			})
			.chain(
				val.title_filter
					.as_ref()
					.map(|t| ("title_filter", t.clone())),
			)
			.chain(val.post_type.as_ref().map(|t| ("post_type", t.to_string())))
			.chain(val.limit.map(|l| ("limit", l.to_string())));
		Url::parse_with_params("http:///search", params).unwrap()
	}
}

pub async fn search(
	db: &Pool<Postgres>,
	search: &Search,
) -> Result<Vec<(String, ArticleMeta)>, sqlx::Error> {
	let result = query!(
		r#"SELECT path as "path!", meta as "meta!" FROM posts
        WHERE path ^@ $1
        AND (meta->>'title') LIKE ('%'||$2||'%')
        AND (meta->'tags') @> $3
        LIMIT $4"#,
		search.search_path,
		search.title_filter.as_ref().map_or("", String::as_str),
		serde_json::to_value(&search.tags).unwrap(),
		search.limit.unwrap_or(i32::MAX as u32) as i32,
	)
	.fetch_all(db)
	.await?;
	let mut result: Vec<_> = result
		.into_iter()
		.filter_map(|r| Some((r.path, serde_json::from_value::<ArticleMeta>(r.meta).ok()?)))
		.filter(|(_, m)| (!m.hidden) || search.ignore_hidden)
		.filter(|(p, m)| {
			!search.exclude_paths.iter().any(|x| p.starts_with(x))
				&& search.created.contains(&m.created)
				&& search.updated.contains(&m.updated)
				&& search.post_type.is_none_or(|t| t == m.post_type)
		})
		.collect();
	result.sort_by(search.sort_type.sort_fn());
	Ok(result)
}

pub async fn guestbook_size(db: &Pool<Postgres>, path: &str) -> Result<i64, sqlx::Error> {
	let result = query!(
		"SELECT COUNT(*) as count FROM guestbook WHERE post = $1",
		path
	)
	.fetch_one(db)
	.await?;
	Ok(result.count.unwrap_or(0))
}

#[derive(Serialize)]
pub struct Signature {
	pub provider: String,
	pub sub: String,
	pub email: String,
	pub name: String,
	pub timestamp: DateTime<Local>,
}

pub async fn read_guestbook(
	db: &Pool<Postgres>,
	path: &str,
) -> Result<Vec<Signature>, sqlx::Error> {
	let result = query_as!(
        Signature,
        "SELECT guests.provider, guests.sub, guests.email, guests.name, guestbook.timestamp FROM guestbook INNER JOIN guests ON guestbook.guest = guests.sub WHERE post = $1 ORDER BY timestamp DESC",
        path
    )
    .fetch_all(db)
    .await?;
	Ok(result)
}

pub async fn add_identity(
	db: &Pool<Postgres>,
	provider: &str,
	identity: Identity,
) -> Result<(), sqlx::Error> {
	query!(
		"INSERT INTO guests VALUES ($1, $2, $3, $4)
            ON CONFLICT (provider, sub)
            DO UPDATE SET
                email = EXCLUDED.email,
                name = EXCLUDED.name;",
		provider,
		identity.sub,
		identity.email,
		identity.name
	)
	.execute(db)
	.await
	.map(|_| ())
}

pub async fn get_identity(
	db: &Pool<Postgres>,
	provider: &str,
	sub: &str,
) -> Result<Option<Identity>, sqlx::Error> {
	query_as!(
		Identity,
		"SELECT sub, email, name FROM guests WHERE provider = $1 AND sub = $2",
		provider,
		sub
	)
	.fetch_optional(db)
	.await
}

pub async fn delete_identity(
	db: &Pool<Postgres>,
	provider: &str,
	sub: &str,
) -> Result<(), sqlx::Error> {
	query!(
		"DELETE FROM guests WHERE provider = $1 AND sub = $2",
		provider,
		sub
	)
	.execute(db)
	.await
	.map(|_| ())
}

pub async fn has_signed(
	db: &Pool<Postgres>,
	provider: &str,
	sub: &str,
) -> Result<bool, sqlx::Error> {
	query!(
		"SELECT COUNT(*) as c FROM guestbook WHERE provider = $1 AND guest = $2",
		provider,
		sub
	)
	.fetch_one(db)
	.await
	.map(|n| n.c.unwrap_or(0) > 0)
}

pub async fn sign_guestbook(
	db: &Pool<Postgres>,
	path: &str,
	provider: &str,
	sub: &str,
) -> Result<(), sqlx::Error> {
	query!(
		"INSERT INTO guestbook VALUES ($1, $2, $3, $4)
            ON CONFLICT (provider, guest, post)
            DO UPDATE SET
                timestamp = EXCLUDED.timestamp",
		provider,
		sub,
		Utc::now(),
		path
	)
	.execute(db)
	.await
	.map(|_| ())
}

pub async fn unsign_guestbook(
	db: &Pool<Postgres>,
	path: &str,
	provider: &str,
	sub: &str,
) -> Result<(), sqlx::Error> {
	query!(
		"DELETE FROM guestbook
        WHERE provider = $1
        AND guest = $2
        AND post = $3",
		provider,
		sub,
		path
	)
	.execute(db)
	.await
	.map(|_| ())
}
