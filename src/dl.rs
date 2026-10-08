use anyhow::{Context, Result};
use futures_util::StreamExt;
use indicatif::ProgressBar;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::api::{self, REFERER};

const RETRIES: usize = 3;

#[derive(Default)]
pub struct Stats {
    pub downloaded: usize,
    pub filtered: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
}

#[derive(PartialEq)]
enum Outcome {
    Downloaded,
    Filtered,
    Skipped,
}

pub async fn download_all(
    dir: &Path,
    urls: &[String],
    min_size: u64,
    concurrency: usize,
    overwrite: bool,
    pb: &ProgressBar,
) -> Stats {
    let http = match api::build_client(None) {
        Ok(c) => c,
        Err(e) => {
            pb.println(format!("初始化 HTTP 客户端失败: {e:#}"));
            return Stats {
                failed: vec![format!("<init>: {e:#}")],
                ..Stats::default()
            };
        }
    };
    download_with(&http, dir, urls, min_size, concurrency, overwrite, pb).await
}

async fn download_with(
    http: &reqwest::Client,
    dir: &Path,
    urls: &[String],
    min_size: u64,
    concurrency: usize,
    overwrite: bool,
    pb: &ProgressBar,
) -> Stats {
    let mut used: HashSet<String> = HashSet::new();
    let tasks: Vec<(String, PathBuf)> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let base = file_name(url);
            let name = if used.insert(base.clone()) {
                base
            } else {
                let unique = format!("{i}_{base}");
                used.insert(unique.clone());
                unique
            };
            (url.clone(), dir.join(name))
        })
        .collect();

    let mut stats = Stats::default();
    let mut stream = futures_util::stream::iter(tasks.into_iter().map(|(url, path)| {
        let http = &http;
        async move {
            match download_one(http, &url, &path, min_size, overwrite).await {
                Ok(outcome) => Ok(outcome),
                Err(e) => Err(format!("{url}: {e:#}")),
            }
        }
    }))
    .buffer_unordered(concurrency.max(1));

    while let Some(res) = stream.next().await {
        match res {
            Ok(Outcome::Downloaded) => stats.downloaded += 1,
            Ok(Outcome::Filtered) => stats.filtered += 1,
            Ok(Outcome::Skipped) => stats.skipped += 1,
            Err(e) => stats.failed.push(e),
        }
        pb.set_message(format!(
            "下载 {} · 过滤 {} · 跳过 {} · 失败 {}",
            stats.downloaded,
            stats.filtered,
            stats.skipped,
            stats.failed.len()
        ));
        pb.inc(1);
    }
    stats
}

async fn download_one(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    min_size: u64,
    overwrite: bool,
) -> Result<Outcome> {
    if !overwrite && let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.len() >= min_size {
            return Ok(Outcome::Skipped);
        }
        let _ = tokio::fs::remove_file(path).await;
    }

    let mut last_err = None;
    for attempt in 1..=RETRIES {
        match try_download(http, url, path, min_size).await {
            Ok(outcome) => return Ok(outcome),
            Err(e) => {
                let _ = tokio::fs::remove_file(part_path(path)).await;
                last_err = Some(e);
                if attempt < RETRIES {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("未知错误")))
}

async fn try_download(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    min_size: u64,
) -> Result<Outcome> {
    let resp = http
        .get(url)
        .header(reqwest::header::REFERER, REFERER)
        .send()
        .await?
        .error_for_status()?;

    let part = part_path(path);
    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("创建 {} 失败", part.display()))?;
    let mut stream = resp.bytes_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    if total < min_size {
        let _ = tokio::fs::remove_file(&part).await;
        return Ok(Outcome::Filtered);
    }

    tokio::fs::rename(&part, path)
        .await
        .with_context(|| format!("重命名 {} 失败", part.display()))?;
    Ok(Outcome::Downloaded)
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".part");
    path.with_file_name(name)
}

fn file_name(url: &str) -> String {
    let raw = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            url.hash(&mut h);
            format!("img_{:x}", h.finish())
        });

    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.').trim();
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Resp, base_url, spawn};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("miyoushe_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_name_uses_last_path_segment() {
        assert_eq!(
            file_name("https://upload-bbs.miyoushe.com/upload/2026/01/abc_123.jpg"),
            "abc_123.jpg"
        );
        assert_eq!(file_name("http://127.0.0.1:1/img.png?x=1&y=2"), "img.png");
    }

    #[test]
    fn file_name_falls_back_when_segment_is_empty() {
        let name = file_name("http://127.0.0.1:1/dir/");
        assert!(name.starts_with("img_"), "{name}");
    }

    #[test]
    fn file_name_falls_back_when_url_is_invalid() {
        let name = file_name("not a url");
        assert!(name.starts_with("img_"), "{name}");
    }

    #[test]
    fn file_name_is_always_a_safe_file_name() {
        let inputs = [
            "http://127.0.0.1:1/a b/c*d.png",
            "http://127.0.0.1:1/名字.jpg",
            "http://127.0.0.1:1/%2e%2e/x.jpg",
            "http://127.0.0.1:1//",
            "https://a.b/x/y/..",
            "not a url",
        ];
        for input in inputs {
            let name = file_name(input);
            assert!(!name.is_empty(), "{input}");
            assert!(
                !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']),
                "{input} -> {name}"
            );
        }
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(part_path(Path::new("b.jpg")), PathBuf::from("b.jpg.part"));
    }

    #[tokio::test]
    async fn downloads_and_keeps_content() {
        let addr = spawn(Arc::new(|_| Resp::bytes(b"0123456789".to_vec()))).await;
        let dir = temp_dir("download_ok");
        let urls = [format!("{}/x/abc.jpg", base_url(addr))];

        let stats =
            download_with(&client(), &dir, &urls, 0, 2, false, &ProgressBar::hidden()).await;

        assert_eq!(stats.downloaded, 1);
        assert_eq!(stats.filtered, 0);
        assert_eq!(stats.skipped, 0);
        assert!(stats.failed.is_empty());
        assert_eq!(std::fs::read(dir.join("abc.jpg")).unwrap(), b"0123456789");
        assert!(!dir.join("abc.jpg.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deletes_file_smaller_than_min_size() {
        let addr = spawn(Arc::new(|_| Resp::bytes(b"0123456789".to_vec()))).await;
        let dir = temp_dir("download_filtered");
        let urls = [format!("{}/x/small.jpg", base_url(addr))];

        let stats = download_with(
            &client(),
            &dir,
            &urls,
            100,
            2,
            false,
            &ProgressBar::hidden(),
        )
        .await;

        assert_eq!(stats.filtered, 1);
        assert_eq!(stats.downloaded, 0);
        assert!(!dir.join("small.jpg").exists());
        assert!(!dir.join("small.jpg.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_existing_file_without_requesting() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            hits_in_handler.fetch_add(1, Ordering::SeqCst);
            Resp::bytes(b"0123456789".to_vec())
        }))
        .await;
        let dir = temp_dir("download_skip");
        std::fs::write(dir.join("abc.jpg"), b"0123456789").unwrap();
        let urls = [format!("{}/x/abc.jpg", base_url(addr))];

        let stats =
            download_with(&client(), &dir, &urls, 0, 2, false, &ProgressBar::hidden()).await;

        assert_eq!(stats.skipped, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn redownloads_when_overwrite_is_set() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            hits_in_handler.fetch_add(1, Ordering::SeqCst);
            Resp::bytes(b"0123456789".to_vec())
        }))
        .await;
        let dir = temp_dir("download_overwrite");
        std::fs::write(dir.join("abc.jpg"), b"old").unwrap();
        let urls = [format!("{}/x/abc.jpg", base_url(addr))];

        let stats = download_with(&client(), &dir, &urls, 0, 2, true, &ProgressBar::hidden()).await;

        assert_eq!(stats.downloaded, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(dir.join("abc.jpg")).unwrap(), b"0123456789");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retries_after_transient_http_error() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            if hits_in_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                Resp::status(500)
            } else {
                Resp::bytes(b"0123456789".to_vec())
            }
        }))
        .await;
        let dir = temp_dir("download_retry");
        let urls = [format!("{}/x/abc.jpg", base_url(addr))];

        let stats =
            download_with(&client(), &dir, &urls, 0, 2, false, &ProgressBar::hidden()).await;

        assert_eq!(stats.downloaded, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reports_failure_after_exhausting_retries() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            hits_in_handler.fetch_add(1, Ordering::SeqCst);
            Resp::status(404)
        }))
        .await;
        let dir = temp_dir("download_404");
        let urls = [format!("{}/x/gone.jpg", base_url(addr))];

        let stats =
            download_with(&client(), &dir, &urls, 0, 2, false, &ProgressBar::hidden()).await;

        assert_eq!(stats.failed.len(), 1);
        assert!(stats.failed[0].contains("gone.jpg"), "{:?}", stats.failed);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert!(!dir.join("gone.jpg").exists());
        assert!(!dir.join("gone.jpg.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn same_file_name_from_different_paths_gets_unique_names() {
        let addr = spawn(Arc::new(|path: &str| {
            if path.starts_with("/x/") {
                Resp::bytes(b"XXX".to_vec())
            } else {
                Resp::bytes(b"YYY".to_vec())
            }
        }))
        .await;
        let dir = temp_dir("download_dedupe");
        let urls = [
            format!("{}/x/same.jpg", base_url(addr)),
            format!("{}/y/same.jpg", base_url(addr)),
        ];

        let stats =
            download_with(&client(), &dir, &urls, 0, 2, false, &ProgressBar::hidden()).await;

        assert_eq!(stats.downloaded, 2);
        assert_eq!(std::fs::read(dir.join("same.jpg")).unwrap(), b"XXX");
        assert_eq!(std::fs::read(dir.join("1_same.jpg")).unwrap(), b"YYY");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
