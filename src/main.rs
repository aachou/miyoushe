mod api;
mod dl;
#[cfg(test)]
mod test_server;

use anyhow::{Context, Result, bail};
use clap::Parser;
use dl::Stats;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "miyoushe",
    about = "爬取米游社用户主页所有帖子中的图片",
    version
)]
struct Args {
    /// 用户 ID 或个人主页链接，如 76438443 或 https://www.miyoushe.com/sr/accountCenter/postList?id=76438443
    target: String,

    /// 输出根目录（其下创建以用户昵称命名的文件夹；默认为系统图片目录）
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// 最小图片大小，支持 500KB / 2MB / 512000，0 表示不过滤
    #[arg(long, default_value = "0")]
    min_size: String,

    /// 并发下载数
    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// 已存在的文件重新下载
    #[arg(long)]
    overwrite: bool,

    /// 只处理前 N 张图片（0 表示全部）
    #[arg(long, default_value_t = 0)]
    limit: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let uid = extract_uid(&args.target)?;
    let min_size = parse_size(&args.min_size)?;

    let api = api::Api::new()?;
    if let Some(proxy) = api::env_proxy_url() {
        println!("使用代理: {proxy}");
    }
    let nickname = match api.nickname(&uid).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("获取用户昵称失败（{e:#}），回退使用 UID");
            uid.clone()
        }
    };
    let out_root = args.output.unwrap_or_else(default_output_dir);
    let dir = out_root.join(sanitize_dir(&nickname, &uid));
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("创建目录 {} 失败", dir.display()))?;

    println!("用户: {nickname} (uid={uid})");
    println!("保存目录: {}", dir.display());

    let images = collect_images(&api, &uid).await?;
    println!("共发现 {} 张图片（去重后）", images.len());
    let images = if args.limit > 0 && args.limit < images.len() {
        println!("按 --limit={} 截取前 {} 张", args.limit, args.limit);
        images.into_iter().take(args.limit).collect()
    } else {
        images
    };
    if min_size > 0 {
        println!("过滤阈值: {min_size} 字节，过小的图片下载后将被删除");
    }
    if images.is_empty() {
        return Ok(());
    }

    let pb = ProgressBar::new(images.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let stats = dl::download_all(
        &dir,
        &images,
        min_size,
        args.concurrency,
        args.overwrite,
        &pb,
    )
    .await;
    pb.finish_and_clear();

    print_summary(&stats, &dir);
    if !stats.failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

async fn collect_images(api: &api::Api, uid: &str) -> Result<Vec<String>> {
    let mut images: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut offsets: HashSet<String> = HashSet::new();
    let mut offset: Option<String> = None;
    let mut pages = 0;

    loop {
        let page = api
            .page(uid, offset.as_deref())
            .await
            .with_context(|| format!("第 {} 页帖子列表请求失败", pages + 1))?;
        pages += 1;
        for url in page.images {
            if seen.insert(url.clone()) {
                images.push(url);
            }
        }
        if page.is_last {
            break;
        }
        let Some(next) = page.next_offset else { break };
        if !offsets.insert(next.clone()) {
            eprintln!("分页偏移重复出现（{next}），提前停止翻页");
            break;
        }
        if pages >= 1000 {
            eprintln!("已达 1000 页上限，提前停止翻页");
            break;
        }
        offset = Some(next);
    }
    Ok(images)
}

fn print_summary(stats: &Stats, dir: &Path) {
    println!("--- 统计 ---");
    println!("下载成功: {}", stats.downloaded);
    println!("小于阈值已删除: {}", stats.filtered);
    println!("已存在跳过: {}", stats.skipped);
    println!("失败: {}", stats.failed.len());
    for e in stats.failed.iter().take(10) {
        println!("  {e}");
    }
    if stats.failed.len() > 10 {
        println!("  ... 另有 {} 条失败", stats.failed.len() - 10);
    }
    println!("文件位于: {}", dir.display());
}

fn extract_uid(target: &str) -> Result<String> {
    let t = target.trim();
    if t.is_empty() {
        bail!("目标不能为空");
    }
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
        return Ok(t.to_string());
    }
    if let Ok(u) = url::Url::parse(t)
        && let Some((_, id)) = u.query_pairs().find(|(k, _)| k == "id")
        && !id.is_empty()
    {
        return Ok(id.into_owned());
    }
    if let Some(pos) = t.find("id=") {
        let rest = &t[pos + 3..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    bail!("无法从 \"{target}\" 解析出用户 ID")
}

fn parse_size(s: &str) -> Result<u64> {
    let s: String = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.to_uppercase();
    if s.is_empty() {
        return Ok(0);
    }
    let cut = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(cut);
    let value: f64 = num
        .parse()
        .with_context(|| format!("无法解析大小数值 \"{num}\""))?;
    let mult: f64 = match unit {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => bail!("不支持的大小单位 \"{unit}\"（可用: B/KB/MB/GB）"),
    };
    Ok((value * mult) as u64)
}

/// 默认输出根目录：系统「图片」目录，取不到时回退当前目录。
fn default_output_dir() -> PathBuf {
    dirs::picture_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn sanitize_dir(nickname: &str, uid: &str) -> String {
    let mut s: String = nickname
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.chars().count() > 60 {
        s = s.chars().take(60).collect();
    }
    let s = s.trim().trim_matches('.').trim().to_string();
    if s.is_empty() || is_windows_reserved(&s) {
        uid.to_string()
    } else {
        s
    }
}

fn is_windows_reserved(name: &str) -> bool {
    let upper = name.to_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base[3..].chars().all(|c| ('1'..='9').contains(&c)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Resp, base_url, spawn};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn api_at(base: String) -> api::Api {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        api::Api::with_client(client, base)
    }

    #[test]
    fn extract_uid_accepts_digits_and_urls() {
        assert_eq!(extract_uid("76438443").unwrap(), "76438443");
        assert_eq!(extract_uid(" 76438443 ").unwrap(), "76438443");
        assert_eq!(
            extract_uid("https://www.miyoushe.com/sr/accountCenter/postList?id=76438443").unwrap(),
            "76438443"
        );
        assert_eq!(
            extract_uid("https://www.miyoushe.com/x?id=1&foo=2").unwrap(),
            "1"
        );
        assert_eq!(
            extract_uid("https://www.miyoushe.com/x?foo=1&id=2").unwrap(),
            "2"
        );
        // 非 URL 的兜底分支
        assert_eq!(extract_uid("xxx?id=55").unwrap(), "55");
    }

    #[test]
    fn extract_uid_rejects_invalid_targets() {
        assert!(extract_uid("").is_err());
        assert!(extract_uid("   ").is_err());
        assert!(extract_uid("abc").is_err());
        assert!(extract_uid("https://www.miyoushe.com/sr/accountCenter/postList?foo=1").is_err());
        assert!(extract_uid("https://www.miyoushe.com/x?id=").is_err());
    }

    #[test]
    fn parse_size_supports_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("").unwrap(), 0);
        assert_eq!(parse_size("512000").unwrap(), 512000);
        assert_eq!(parse_size("512000B").unwrap(), 512000);
        assert_eq!(parse_size("500KB").unwrap(), 500 * 1024);
        assert_eq!(parse_size("500kb").unwrap(), 500 * 1024);
        assert_eq!(parse_size(" 500 KB ").unwrap(), 500 * 1024);
        assert_eq!(parse_size("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1.5MB").unwrap(), 1536 * 1024);
    }

    #[test]
    fn parse_size_rejects_bad_input() {
        assert!(parse_size("500XYZ").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("MB").is_err());
    }

    #[test]
    fn sanitize_dir_cleans_invalid_characters() {
        assert_eq!(sanitize_dir("正常昵称", "1"), "正常昵称");
        assert_eq!(
            sanitize_dir("a/b\\c:d*e?f\"g<h>i|j", "1"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_dir("  name. ", "1"), "name");
    }

    #[test]
    fn sanitize_dir_falls_back_to_uid() {
        assert_eq!(sanitize_dir("", "123"), "123");
        assert_eq!(sanitize_dir("   ", "123"), "123");
        assert_eq!(sanitize_dir("...", "123"), "123");
        assert_eq!(sanitize_dir("CON", "123"), "123");
        assert_eq!(sanitize_dir("com1.txt", "123"), "123");
        assert_eq!(sanitize_dir("LPT9", "123"), "123");
    }

    #[test]
    fn sanitize_dir_truncates_long_names() {
        let long = "长".repeat(100);
        let out = sanitize_dir(&long, "123");
        assert_eq!(out.chars().count(), 60);
    }

    #[test]
    fn default_output_dir_is_pictures_or_cwd() {
        let dir = default_output_dir();
        assert!(!dir.as_os_str().is_empty());
        match dirs::picture_dir() {
            Some(pics) => assert_eq!(dir, pics),
            None => assert_eq!(dir, PathBuf::from(".")),
        }
    }

    #[test]
    fn windows_reserved_names_are_detected() {
        for name in ["CON", "con", "PRN", "AUX", "NUL", "COM1", "LPT9", "con.txt"] {
            assert!(is_windows_reserved(name), "{name}");
        }
        for name in ["COM0", "COM10", "CONSOLE", "AUXILIARY", "normal"] {
            assert!(!is_windows_reserved(name), "{name}");
        }
    }

    #[tokio::test]
    async fn collect_images_walks_pages_and_dedupes() {
        let addr = spawn(Arc::new(|path: &str| {
            if path.contains("offset=20") {
                Resp::json(
                    r#"{"retcode":0,"message":"OK","data":{
                        "is_last":true,
                        "list":[
                            {"post":{"images":["http://img/3.jpg","http://img/1.jpg"]}}
                        ]}}"#,
                )
            } else {
                Resp::json(
                    r#"{"retcode":0,"message":"OK","data":{
                        "is_last":false,
                        "next_offset":"20",
                        "list":[
                            {"post":{"images":["http://img/1.jpg","http://img/2.jpg"]}}
                        ]}}"#,
                )
            }
        }))
        .await;

        let images = collect_images(&api_at(base_url(addr)), "1").await.unwrap();
        assert_eq!(
            images,
            ["http://img/1.jpg", "http://img/2.jpg", "http://img/3.jpg"]
        );
    }

    #[tokio::test]
    async fn collect_images_stops_on_repeated_offset() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            hits_in_handler.fetch_add(1, Ordering::SeqCst);
            Resp::json(
                r#"{"retcode":0,"message":"OK","data":{
                    "is_last":false,
                    "next_offset":"20",
                    "list":[{"post":{"images":["http://img/1.jpg"]}}]}}"#,
            )
        }))
        .await;

        let images = collect_images(&api_at(base_url(addr)), "1").await.unwrap();
        assert_eq!(images, ["http://img/1.jpg"]);
        // 第一页返回 offset=20，第二页仍返回 20，触发重复偏移保护
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn collect_images_handles_empty_account() {
        let addr = spawn(Arc::new(|_| {
            Resp::json(r#"{"retcode":0,"message":"OK","data":{"is_last":true,"list":[]}}"#)
        }))
        .await;

        let images = collect_images(&api_at(base_url(addr)), "1").await.unwrap();
        assert!(images.is_empty());
    }
}
