use crate::args;
use crate::db;
use crate::errors::*;
use crate::ingest;
use crate::sbom;
use diffy_fork_filenames as diffy;
use log::error;
use num_format::{Locale, ToFormattedString};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::Infallible;
use std::result;
use std::sync::Arc;
use tokio::task::JoinSet;
use warp::reject;
use warp::{
    http::{header, HeaderValue, StatusCode},
    Filter,
};

const SEARCH_LIMIT: usize = 150;

const CACHE_CONTROL_DEFAULT: HeaderValue =
    HeaderValue::from_static("max-age=600, stale-while-revalidate=300, stale-if-error=300");
const CACHE_CONTROL_SHORT: HeaderValue =
    HeaderValue::from_static("max-age=10, stale-while-revalidate=20, stale-if-error=60");

#[derive(RustEmbed)]
#[folder = "templates"]
#[include = "*.hbs"]
#[include = "*.css"]
struct Assets;

struct Handlebars<'a> {
    hbs: handlebars::Handlebars<'a>,
}

handlebars::handlebars_helper!(format_num: |v: i64, width: i64| {
    let v = v.to_formatted_string(&Locale::en);
    format!("{:>width$}", v, width=width as usize)
});

handlebars::handlebars_helper!(pad_right: |v: String, width: i64| {
    format!("{:<width$}", v, width=width as usize)
});

impl<'a> Handlebars<'a> {
    fn new() -> Result<Handlebars<'a>> {
        let mut hbs = handlebars::Handlebars::new();
        hbs.set_prevent_indent(true);
        hbs.register_embed_templates::<Assets>()?;
        hbs.register_helper("format_num", Box::new(format_num));
        hbs.register_helper("pad_right", Box::new(pad_right));
        Ok(Handlebars { hbs })
    }

    fn render<T>(&self, name: &str, data: &T) -> Result<String>
    where
        T: serde::Serialize,
    {
        let out = self.hbs.render(name, data)?;
        Ok(out)
    }

    fn render_archive(&self, artifact: &db::Artifact) -> Result<String> {
        let artifact = self.hbs.render("archive.txt.hbs", artifact)?;
        Ok(artifact)
    }
}

fn cache_control(reply: impl warp::Reply, value: HeaderValue) -> impl warp::Reply {
    warp::reply::with_header(reply, header::CACHE_CONTROL, value)
}

async fn index(hbs: Arc<Handlebars<'_>>) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let html = hbs.render("index.html.hbs", &()).map_err(Error::from)?;
    Ok(Box::new(warp::reply::html(html)))
}

fn detect_autotools(artifact: &db::Artifact) -> Result<bool> {
    let Some(files) = &artifact.files else {
        return Ok(false);
    };
    let files = serde_json::from_value::<Vec<ingest::tar::Entry>>(files.clone())?;

    let mut configure = HashSet::new();
    let mut configure_ac = HashSet::new();

    for file in &files {
        if let Some(folder) = file.path.strip_suffix("/configure") {
            if configure_ac.contains(folder) {
                return Ok(true);
            }
            configure.insert(folder);
        }
        if let Some(folder) = file.path.strip_suffix("/configure.ac") {
            if configure.contains(folder) {
                return Ok(true);
            }
            configure_ac.insert(folder);
        }
    }

    Ok(false)
}

async fn artifact(
    hbs: Arc<Handlebars<'_>>,
    db: Arc<db::Client>,
    chksum: String,
) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let (chksum, json) = chksum
        .strip_suffix(".json")
        .map(|chksum| (chksum, true))
        .unwrap_or((chksum.as_str(), false));

    let alias = db.get_artifact_alias(chksum).await?;

    let resolved_chksum = alias
        .as_ref()
        .map(|a| a.alias_to.as_str())
        .unwrap_or(chksum);
    let Some(artifact) = db.get_artifact(resolved_chksum).await? else {
        return Err(reject::not_found());
    };

    let sbom_refs = db.get_sbom_refs_for_archive(resolved_chksum).await?;

    if json {
        Ok(Box::new(warp::reply::json(&json!({
            "files": artifact.files,
            "sbom_refs": sbom_refs,
        }))))
    } else {
        let refs = db.get_all_refs_for(&artifact.chksum).await?;
        let files = hbs.render_archive(&artifact)?;

        let suspecting_autotools = detect_autotools(&artifact)?;

        let html = hbs
            .render(
                "artifact.html.hbs",
                &json!({
                    "artifact": artifact,
                    "chksum": chksum,
                    "alias": alias,
                    "refs": refs,
                    "sbom_refs": sbom_refs,
                    "files": files,
                    "suspecting_autotools": suspecting_autotools,
                }),
            )
            .map_err(Error::from)?;
        Ok(Box::new(warp::reply::html(html)))
    }
}

async fn sbom(
    hbs: Arc<Handlebars<'_>>,
    db: Arc<db::Client>,
    chksum: String,
) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let (chksum, txt) = chksum
        .strip_suffix(".txt")
        .map(|chksum| (chksum, true))
        .unwrap_or((chksum.as_str(), false));

    let Some(sbom) = db.get_sbom(chksum).await? else {
        return Err(reject::not_found());
    };

    let sbom_refs = db.get_sbom_refs_for_sbom(&sbom).await?;

    if txt {
        let mut res = warp::reply::Response::new(sbom.data.into());
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        Ok(Box::new(res))
    } else {
        let packages = match sbom::Sbom::try_from(&sbom).and_then(|sbom| sbom.to_packages()) {
            Ok(packages) => packages,
            Err(err) => {
                warn!("Failed to parse package lock: {err:#}");
                Vec::new()
            }
        };

        let html = hbs
            .render(
                "sbom.html.hbs",
                &json!({
                    "sbom": sbom,
                    "chksum": chksum,
                    "sbom_refs": sbom_refs,
                    "packages": packages,
                }),
            )
            .map_err(Error::from)?;
        Ok(Box::new(warp::reply::html(html)))
    }
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    hbs: Arc<Handlebars<'_>>,
    db: Arc<db::Client>,
    search: SearchQuery,
) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let mut query = search.q.clone();
    query.retain(|c| !"%_".contains(c));
    query.push('%');

    let refs = db.search(&query, SEARCH_LIMIT).await?;

    let html = hbs
        .render(
            "search.html.hbs",
            &json!({
                "search": search.q,
                "refs": refs,
            }),
        )
        .map_err(Error::from)?;
    Ok(Box::new(warp::reply::html(html)))
}

async fn stats(
    hbs: Arc<Handlebars<'_>>,
    db: Arc<db::Client>,
) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let mut set = JoinSet::new();
    {
        let db = db.clone();
        set.spawn(async move { ("import_dates", db.stats_import_dates().await) });
    }
    set.spawn(async move { ("pending_tasks", db.stats_pending_tasks().await) });

    let mut data = HashMap::new();
    while let Some(row) = set.join_next().await {
        let Ok((key, values)) = row else { continue };
        let values = values?;

        // for import_dates, also calculate and insert sum
        if key == "import_dates" {
            let sum = values.iter().map(|(_, v)| v).sum();
            data.insert("total_artifacts", vec![("".to_string(), sum)]);
        }

        // add regular data
        data.insert(key, values);
    }

    let html = hbs.render("stats.html.hbs", &data).map_err(Error::from)?;
    Ok(Box::new(warp::reply::html(html)))
}

fn process_files_list(
    value: Option<serde_json::Value>,
    trimmed: bool,
) -> Result<Option<serde_json::Value>> {
    let Some(value) = value else { return Ok(None) };
    let mut list = serde_json::from_value::<Vec<ingest::tar::Entry>>(value)?;
    if trimmed {
        for item in &mut list {
            item.path = item
                .path
                .split_once('/')
                .map(|(_a, b)| b)
                .unwrap_or(&item.path)
                .to_string();
        }
    }
    list.sort_by(|a, b| a.path.partial_cmp(&b.path).unwrap());
    let value = serde_json::to_value(&list)?;
    Ok(Some(value))
}

async fn diff(
    hbs: Arc<Handlebars<'_>>,
    db: Arc<db::Client>,
    diff_from: String,
    diff_to: String,
    sorted: bool,
    trimmed: bool,
) -> result::Result<Box<dyn warp::Reply>, warp::Rejection> {
    let Some(mut artifact1) = db.resolve_artifact(&diff_from).await? else {
        return Err(reject::not_found());
    };

    let Some(mut artifact2) = db.resolve_artifact(&diff_to).await? else {
        return Err(reject::not_found());
    };

    if sorted {
        artifact1.files = process_files_list(artifact1.files, trimmed)?;
        artifact2.files = process_files_list(artifact2.files, trimmed)?;
    }

    let artifact1 = hbs.render_archive(&artifact1)?;
    let artifact2 = hbs.render_archive(&artifact2)?;

    let diff = diffy::create_file_patch(&artifact1, &artifact2, &diff_from, &diff_to);
    let diff = diff.to_string();

    let html = hbs
        .render(
            "diff.html.hbs",
            &json!({
                "diff": diff,
                "diff_from": diff_from,
                "diff_to": diff_to,
                "sorted": sorted,
                "trimmed": trimmed,
            }),
        )
        .map_err(Error::from)?;
    Ok(Box::new(warp::reply::html(html)))
}

pub async fn rejection(err: warp::Rejection) -> result::Result<impl warp::Reply, Infallible> {
    let code;
    let message;

    if err.is_not_found() {
        code = StatusCode::NOT_FOUND;
        message = "404 - file not found\n";
    } else {
        error!("unhandled rejection: {:?}", err);
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = "server error\n";
    }

    Ok(warp::reply::with_status(message, code))
}

pub async fn run(args: &args::Web) -> Result<()> {
    let hbs = Arc::new(Handlebars::new()?);
    let hbs = warp::any().map(move || hbs.clone());

    let db = db::Client::create().await?;
    let db = Arc::new(db);
    let db = warp::any().map(move || db.clone());

    let index = warp::get()
        .and(hbs.clone())
        .and(warp::path::end())
        .and_then(index)
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let artifact = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("artifact"))
        .and(warp::path::param())
        .and(warp::path::end())
        .and_then(artifact)
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let sbom = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("sbom"))
        .and(warp::path::param())
        .and(warp::path::end())
        .and_then(sbom)
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let search = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("search"))
        .and(warp::path::end())
        .and(warp::query::<SearchQuery>())
        .and_then(search)
        .map(|r| cache_control(r, CACHE_CONTROL_SHORT));
    let stats = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("stats"))
        .and(warp::path::end())
        .and_then(stats)
        .map(|r| cache_control(r, CACHE_CONTROL_SHORT));
    let diff_original = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("diff"))
        .and(warp::path::param())
        .and(warp::path::param())
        .and(warp::path::end())
        .and_then(|hbs, db, diff_from, diff_to| diff(hbs, db, diff_from, diff_to, false, false))
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let diff_sorted = warp::get()
        .and(hbs.clone())
        .and(db.clone())
        .and(warp::path("diff-sorted"))
        .and(warp::path::param())
        .and(warp::path::param())
        .and(warp::path::end())
        .and_then(|hbs, db, diff_from, diff_to| diff(hbs, db, diff_from, diff_to, true, false))
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let diff_sorted_trimmed = warp::get()
        .and(hbs)
        .and(db)
        .and(warp::path("diff-sorted-trimmed"))
        .and(warp::path::param())
        .and(warp::path::param())
        .and(warp::path::end())
        .and_then(|hbs, db, diff_from, diff_to| diff(hbs, db, diff_from, diff_to, true, true))
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));
    let style = warp::get()
        .and(warp::path("assets"))
        .and(warp::path("style.css"))
        .and(warp::path::end())
        .and(warp_embed::embed_one(&Assets, "style.css"))
        .map(|r| cache_control(r, CACHE_CONTROL_DEFAULT));

    let routes = warp::any()
        .and(
            index
                .or(artifact)
                .or(sbom)
                .or(search)
                .or(stats)
                .or(diff_original)
                .or(diff_sorted)
                .or(diff_sorted_trimmed)
                .or(style),
        )
        .recover(rejection);

    warp::serve(routes).run(args.bind_addr).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::tar::LinksTo;
    use sqlx::types::chrono::Utc;

    #[test]
    fn test_render_archive() {
        let hbs = Handlebars::new().unwrap();
        let out = hbs.render_archive(&db::Artifact {
            chksum: "abcd".to_string(),
            first_seen: Utc::now(),
            last_imported: Utc::now(),
            files: Some(serde_json::to_value([
                ingest::tar::Entry {
                    digest: None,
                    path: "cmatrix-2.0/".to_string(),
                    links_to: None,
                },
                ingest::tar::Entry {
                    digest: Some("sha256:45705163f227f0b5c20dc79e3d3e41b4837cb968d1c3af60cc6301b577038984".to_string()),
                    path: "cmatrix-2.0/.gitignore".to_string(),
                    links_to: None,
                },
                ingest::tar::Entry {
                    digest: None,
                    path: "cmatrix-2.0/data/".to_string(),
                    links_to: None,
                },
                ingest::tar::Entry {
                    digest: None,
                    path: "cmatrix-2.0/data/img/".to_string(),
                    links_to: None,
                },
                ingest::tar::Entry {
                    digest: Some("sha256:ffa566a67628191d5450b7209d6f08c8867c12380d3ebc9e808dc4012e3aca58".to_string()),
                    path: "cmatrix-2.0/data/img/capture_bold_font.png".to_string(),
                    links_to: None,
                }
            ]).unwrap()),
        }).unwrap();
        assert_eq!(out, "                                                                         cmatrix-2.0/
sha256:45705163f227f0b5c20dc79e3d3e41b4837cb968d1c3af60cc6301b577038984  cmatrix-2.0/.gitignore
                                                                         cmatrix-2.0/data/
                                                                         cmatrix-2.0/data/img/
sha256:ffa566a67628191d5450b7209d6f08c8867c12380d3ebc9e808dc4012e3aca58  cmatrix-2.0/data/img/capture_bold_font.png
");
    }

    #[test]
    fn test_render_archive_symlink() {
        let hbs = Handlebars::new().unwrap();
        let out = hbs
            .render_archive(&db::Artifact {
                chksum: "abcd".to_string(),
                first_seen: Utc::now(),
                last_imported: Utc::now(),
                files: Some(
                    serde_json::to_value([
                        ingest::tar::Entry {
                            digest: None,
                            path: "foo-1.0/".to_string(),
                            links_to: None,
                        },
                        ingest::tar::Entry {
                            digest: Some("sha256:56d9fc4585da4f39bbc5c8ec953fb7962188fa5ed70b2dd5a19dc82df997ba5e".to_string()),
                            path: "foo-1.0/original_file".to_string(),
                            links_to: None,
                        },
                        ingest::tar::Entry {
                            digest: None,
                            path: "foo-1.0/symlink_file".to_string(),
                            links_to: Some(LinksTo::Symbolic("original_file".to_string())),
                        },
                    ])
                    .unwrap(),
                ),
            })
            .unwrap();
        assert_eq!(
            out,
            "                                                                         foo-1.0/
sha256:56d9fc4585da4f39bbc5c8ec953fb7962188fa5ed70b2dd5a19dc82df997ba5e  foo-1.0/original_file
                                                                         foo-1.0/symlink_file -> original_file
"
        );
    }

    #[test]
    fn test_render_archive_hardlink() {
        let hbs = Handlebars::new().unwrap();
        let out = hbs
            .render_archive(&db::Artifact {
                chksum: "abcd".to_string(),
                first_seen: Utc::now(),
                last_imported: Utc::now(),
                files: Some(
                    serde_json::to_value([
                        ingest::tar::Entry {
                            digest: None,
                            path: "foo-1.0/".to_string(),
                            links_to: None,
                        },
                        ingest::tar::Entry {
                            digest: Some("sha256:56d9fc4585da4f39bbc5c8ec953fb7962188fa5ed70b2dd5a19dc82df997ba5e".to_string()),
                            path: "foo-1.0/original_file".to_string(),
                            links_to: None,
                        },
                        ingest::tar::Entry {
                            digest: None,
                            path: "foo-1.0/hardlink_file".to_string(),
                            links_to: Some(LinksTo::Hard("foo-1.0/original_file".to_string())),
                        },
                    ])
                    .unwrap(),
                ),
            })
            .unwrap();
        assert_eq!(
            out,
            "                                                                         foo-1.0/
sha256:56d9fc4585da4f39bbc5c8ec953fb7962188fa5ed70b2dd5a19dc82df997ba5e  foo-1.0/original_file
                                                                         foo-1.0/hardlink_file link to foo-1.0/original_file
"
        );
    }
}
