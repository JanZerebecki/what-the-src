use crate::args;
use crate::chksums::{Checksums, Hasher};
use crate::db;
use crate::errors::*;
use crate::ingest;
use crate::utils;
use futures::StreamExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{self, AsyncRead};
use tokio::process::Command;
use tokio_tar::{Archive, EntryType};

pub async fn read_routine<R: AsyncRead + Unpin>(
    db: &db::Client,
    reader: R,
    vendor: String,
    package: String,
    version: String,
) -> Result<()> {
    let mut tar = Archive::new(reader);
    let mut entries = tar.entries()?;

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let filename = {
            let path = entry.path()?;
            debug!("Found entry in .rpm: {:?}", path);

            if entry.header().entry_type() != EntryType::Regular {
                continue;
            }

            let Some(filename) = path.file_name() else {
                continue;
            };
            let Some(filename) = filename.to_str() else {
                continue;
            };

            filename.to_string()
        };

        // TODO: find a better solution for this, can we just autodetect all regardless of file name?
        let archive_w_compression = if filename.ends_with(".tar.gz")
            || filename.ends_with(".tgz")
            || filename.ends_with(".crate")
        {
            Some(Some("gz"))
        } else if filename.ends_with(".tar.xz") {
            Some(Some("xz"))
        } else if filename.ends_with(".tar.bz2") {
            Some(Some("bz2"))
        } else if filename.ends_with(".tar") {
            Some(None)
        } else {
            None;
        };

        let chksum = match archive_w_compression {
            Some(compression) => {
                // in case of chromium, calculate the checksum but do not import
                let tar_db = if filename.starts_with("chromium-") {
                    None
                } else {
                    Some(db)
                };
                let summary = ingest::tar::stream_data(tar_db, entry, compression).await?;
                summary.outer_digests.sha256.clone()
            }
            None => {
                let hasher = Hasher::new(entry).await?;
                hasher.digests.sha256.clone()
            }
        };

        let r = db::Ref {
            chksum: chksum,
            vendor: vendor.to_string(),
            package: package.to_string(),
            version: version.to_string(),
            filename: Some(filename.to_string()),
        };
        info!("insert ref: {r:?}");
        db.insert_ref(&r).await?;
    }
    Ok(())
}

pub async fn stream_data<R: AsyncRead + Unpin>(
    db: Arc<db::Client>,
    mut reader: R,
    vendor: String,
    package: String,
    version: String,
) -> Result<()> {
    let mut child = Command::new("bsdtar")
        .args(["-c", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let writer = async {
        let n = io::copy(&mut reader, &mut stdin).await;
        drop(stdin);
        n
    };

    let stdout = child.stdout.take().unwrap();
    let reader =
        tokio::spawn(async move { read_routine(&db, stdout, vendor, package, version).await });

    let (reader, writer) = tokio::join!(reader, writer);
    debug!("Sent {} bytes to child process", writer?);
    let status = child.wait().await?;
    if !status.success() {
        return Err(Error::ChildExit(status));
    }
    debug!("Finished processing .rpm");
    reader?
}

pub async fn run(args: &args::IngestRpm) -> Result<()> {
    let db = db::Client::create().await?;
    let db = Arc::new(db);

    let reader = utils::fetch_or_open(&args.file, args.fetch).await?;
    stream_data(
        db.clone(),
        reader,
        args.vendor.to_string(),
        args.package.to_string(),
        args.version.to_string(),
    )
    .await?;

    Ok(())
}
