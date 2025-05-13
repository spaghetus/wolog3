use std::{
    collections::{BTreeMap, HashSet},
    fs::Permissions,
    io::{stderr, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

use pandoc_ast::{Block, Format, Inline, MetaValue, MutVisitor, Pandoc};
use serde_json::json;
use sqlx::{query, Pool, Postgres};
use tempfile::NamedTempFile;
use tera::{Context, Tera};
use tokio::{io::AsyncWriteExt, runtime::Handle, sync::RwLock};
use url::Url;

use crate::{
    cookies::ClientPersist,
    db::{PostType, Search},
};

pub async fn md_to_ast(file: &impl AsRef<Path>) -> Option<Pandoc> {
    let file = file.as_ref();
    let pandoc = tokio::process::Command::new("pandoc")
        .args(["-fmarkdown", "-tjson"])
        .arg(file.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .output()
        .await
        .inspect_err(|e| eprintln!("Failed to get pandoc ast from {file:?} with {e}"))
        .ok()?;
    if !pandoc.status.success() {
        return None;
    }
    let pandoc = String::from_utf8(pandoc.stdout).ok()?;
    Some(Pandoc::from_json(&pandoc))
}

pub async fn ast_to_html(ast: Pandoc) -> Option<String> {
    let mut pandoc = tokio::process::Command::new("pandoc")
        .args(["-fjson", "-thtml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    pandoc
        .stdin
        .as_mut()
        .unwrap()
        .write_all(serde_json::to_string(&ast).unwrap().as_bytes())
        .await
        .ok()?;
    let pandoc = pandoc.wait_with_output().await.ok()?;
    if !pandoc.status.success() {
        return None;
    }
    let pandoc = String::from_utf8(pandoc.stdout).ok()?;
    Some(pandoc)
}

pub async fn run_preproc_filters(db: &Pool<Postgres>, ast: Pandoc, _path: &str) -> Pandoc {
    let ast = find_links(ast).await;
    let ast = dynamic(ast).await;
    ast
}

pub async fn run_postproc_filters(
    db: &Pool<Postgres>,
    tera: &Arc<RwLock<Tera>>,
    ast: Pandoc,
    path: &str,
    cookie: &ClientPersist,
) -> Pandoc {
    // let ast = attach_mentioners(db, ast, path).await;
    let ast = include(db, tera, ast).await;
    let ast = frag_search_results(db, tera, ast, &cookie).await;
    ast
}

async fn find_links(mut ast: Pandoc) -> Pandoc {
    struct LinkVisitor(Vec<String>, PostType);
    impl MutVisitor for LinkVisitor {
        fn visit_inline(&mut self, inline: &mut Inline) {
            if let Inline::Link((_, classes, _), _contents, (target, _)) = inline {
                if classes.iter().any(|c| c == "mention") {
                    self.0.push(target.to_string());
                    for class in classes.iter() {
                        if matches!(self.1, PostType::Note) {
                            self.1 = match class.as_str() {
                                "u-like-of" => PostType::Like,
                                "u-repost-of" => PostType::Repost,
                                "u-in-reply-to" => PostType::Reply,
                                _ => PostType::Note,
                            }
                        }
                    }
                }
            }
            self.walk_inline(inline)
        }
    }
    let mut visitor = LinkVisitor(vec![], PostType::Note);
    visitor.walk_pandoc(&mut ast);
    let LinkVisitor(mut mentions, mut post_type) = visitor;
    if matches!(post_type, PostType::Note) && ast.meta.contains_key("title") {
        post_type = PostType::Article;
    }
    mentions.push("https://fed.brid.gy/".to_string());
    let mut mentions: Vec<_> = mentions.into_iter().map(MetaValue::MetaString).collect();
    if let Some(MetaValue::MetaList(existing_mentions)) = ast.meta.get("mentions") {
        mentions.extend(existing_mentions.iter().cloned());
    }
    ast.meta
        .insert("mentions".to_string(), MetaValue::MetaList(mentions));
    ast.meta.insert(
        "post_type".to_string(),
        MetaValue::MetaString(post_type.to_string()),
    );
    if !ast.meta.contains_key("template") {
        ast.meta.insert(
            "template".to_string(),
            MetaValue::MetaString(
                match post_type {
                    PostType::Note => "note",
                    PostType::Article => "article",
                    PostType::Like => "like",
                    PostType::Repost => "repost",
                    PostType::Reply => "reply",
                }
                .to_string(),
            ),
        );
    }
    ast
}

async fn dynamic(mut ast: Pandoc) -> Pandoc {
    struct DynamicVisitor;
    impl MutVisitor for DynamicVisitor {
        fn visit_block(&mut self, block: &mut Block) {
            if let Block::CodeBlock((_, classes, attr), contents) = block {
                if !classes.iter().any(|c| c == "dynamic") {
                    return;
                }
                let json = classes.iter().any(|c| c == "pandoc_ast");
                let interpreter = attr
                    .iter()
                    .find(|(k, _)| k == "interpreter")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("bash");
                let mut file = NamedTempFile::new().unwrap();
                file.write_all(contents.as_bytes()).unwrap();
                file.flush().unwrap();
                std::fs::set_permissions(&file, Permissions::from_mode(0o005)).unwrap();
                let output = Command::new("sudo")
                    .args(["-u", "nobody", "-g", "nogroup", interpreter])
                    .arg(file.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .unwrap();
                if !output.status.success() {
                    eprintln!("Code block failed with code {:?}", output.status.code());
                    eprintln!("stdout:");
                    stderr().write_all(&output.stdout).unwrap();
                    eprintln!("stderr:");
                    stderr().write_all(&output.stderr).unwrap();
                    *contents = "Code block failed to execute, check server logs.".to_string();
                    return;
                }
                if json {
                    *block = serde_json::from_slice(&output.stdout).unwrap();
                } else {
                    let output = String::from_utf8_lossy(&output.stdout);
                    *contents = output.to_string()
                }
            }
            self.walk_block(block);
        }
    }
    tokio::task::spawn_blocking(move || {
        DynamicVisitor.walk_pandoc(&mut ast);
        ast
    })
    .await
    .unwrap()
}

async fn frag_search_results(
    db: &Pool<Postgres>,
    tera: &Arc<RwLock<Tera>>,
    mut ast: Pandoc,
    cookie: &ClientPersist,
) -> Pandoc {
    struct FragSearchVisitor(Handle, Pool<Postgres>, Arc<RwLock<Tera>>, ClientPersist);
    impl MutVisitor for FragSearchVisitor {
        fn visit_block(&mut self, block: &mut Block) {
            if let Block::CodeBlock((_, classes, _), contents) = block {
                if !classes.iter().any(|c| c == "search") {
                    return;
                }

                let Ok(search_spec): Result<Search, _> = serde_yml::from_str(contents) else {
                    eprintln!("Bad search block {contents}");
                    return;
                };

                let Ok(search) = self.0.block_on(crate::db::search(&self.1, &search_spec)) else {
                    eprintln!("Search failed: {search_spec:#?}");
                    return;
                };

                let search_url: Url = (&search_spec).into();

                let ctx = json!({
                    "articles": search,
                    "search_qs": search_url.query().unwrap_or(""),
                    "cookie": &self.3,
                    "new": search.iter().filter(|(path, meta)| {
                        let Some(viewed) = self.3.viewed.get(path) else {return true};
                        *viewed < meta.updated
                    }).map(|(p, _)| p).collect::<HashSet<_>>(),
                    "search": search_spec,
                });
                let ctx = Context::from_serialize(ctx).unwrap();

                let html = self
                    .2
                    .blocking_read()
                    .render("frag-search-results.html.tera", &ctx)
                    .unwrap_or_else(|e| format!("Search template failure: {e:#?}"));
                *block = Block::RawBlock(Format("html".to_string()), html);
            } else {
                self.walk_block(block);
            }
        }
    }
    let mut visitor =
        FragSearchVisitor(Handle::current(), db.clone(), tera.clone(), cookie.clone());
    let ast = tokio::task::spawn_blocking(move || {
        visitor.walk_pandoc(&mut ast);
        ast
    })
    .await
    .unwrap();

    ast
}

async fn include(db: &Pool<Postgres>, tera: &Arc<RwLock<Tera>>, mut ast: Pandoc) -> Pandoc {
    #[derive(serde::Deserialize)]
    struct Include {
        src: String,
        #[serde(default)]
        headings: Vec<String>,
    }
    struct IncludeVisitor(Handle, Pool<Postgres>);
    impl MutVisitor for IncludeVisitor {
        fn visit_block(&mut self, block: &mut Block) {
            self.walk_block(block);
            if let Block::CodeBlock((_, classes, _), contents) = block {
                if !classes.iter().any(|c| c == "include") {
                    return;
                }

                let Ok(include_spec): Result<Include, _> = serde_yml::from_str(contents) else {
                    eprintln!("Bad include block {contents}");
                    return;
                };

                let Ok(Some(article)) = self.0.block_on(
                    query!(
                        r#"SELECT ast as "ast: sqlx::types::Json<pandoc_ast::Pandoc>" FROM posts WHERE path = $1"#,
                        include_spec.src
                    )
                    .fetch_optional(&self.1),
                ) else {
                    eprintln!("Failing include block {contents}");
                    return;
                };
                let article = article.ast;

                let mut blocks = if include_spec.headings.is_empty() {
                    article.blocks.clone()
                } else {
                    let mut taking = None;
                    dbg!(&include_spec.headings);
                    article
                        .blocks
                        .iter()
                        .flat_map(|b| {
                            dbg!(b);
                            match (taking, b) {
                                (Option::None, b @ Block::Header(level, (id, _, _), _))
                                    if include_spec.headings.iter().any(|i| i.eq(id.as_str())) =>
                                {
                                    taking = Some(level);
                                    Some(b)
                                }
                                (Some(tl), b @ Block::Header(level, (id, _, _), _))
                                    if tl <= level
                                        && !include_spec
                                            .headings
                                            .iter()
                                            .any(|i| i.eq(id.as_str())) =>
                                {
                                    taking = None;
                                    None
                                }
                                (Some(_), b) => Some(b),
                                (Option::None, _) => None,
                            }
                        })
                        .cloned()
                        .collect()
                };
                blocks.push(Block::LineBlock(vec![vec![Inline::Link(
                    Default::default(),
                    vec![Inline::Str("(copied from another page)".to_string())],
                    (format!("/post/{}", include_spec.src), String::new()),
                )]]));
                *block = Block::BlockQuote(blocks);
            }
        }
    }
    let mut visitor = IncludeVisitor(Handle::current(), db.clone());
    let ast = tokio::task::spawn_blocking(move || {
        visitor.walk_pandoc(&mut ast);
        ast
    })
    .await
    .unwrap();

    ast
}

async fn attach_mentioners(db: &Pool<Postgres>, mut ast: Pandoc, path: &str) -> Pandoc {
    let MetaValue::MetaList(list) = ast
        .meta
        .entry("mentioners".to_string())
        .or_insert(MetaValue::MetaList(vec![]))
    else {
        return ast;
    };
    dbg!(&list);

    let Ok(mentions) = crate::db::mentioners(db, path).await else {
        return ast;
    };

    list.extend(
        mentions
            .into_iter()
            .map(|m| MetaValue::MetaString(m.from_url)),
    );

    ast
}
