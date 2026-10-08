use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

pub const API_BASE: &str = "https://bbs-api.mihoyo.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
pub const REFERER: &str = "https://www.miyoushe.com/";

const PAGE_SIZE: usize = 20;
const RETRIES: usize = 3;

#[derive(Deserialize)]
struct Envelope<T> {
    retcode: i64,
    message: String,
    data: Option<T>,
}

#[derive(Deserialize)]
struct UserInfoData {
    user_info: UserInfo,
}

#[derive(Deserialize)]
struct UserInfo {
    nickname: String,
}

#[derive(Deserialize)]
struct UserPostListData {
    #[serde(default)]
    list: Vec<PostItem>,
    is_last: bool,
    next_offset: Option<String>,
}

#[derive(Deserialize)]
struct PostItem {
    post: Post,
}

#[derive(Deserialize)]
struct Post {
    #[serde(default)]
    images: Vec<String>,
}

pub struct Api {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug)]
pub struct Page {
    pub images: Vec<String>,
    pub is_last: bool,
    pub next_offset: Option<String>,
}

impl Api {
    pub fn new() -> Result<Self> {
        Self::with_base(API_BASE)
    }

    pub fn with_base(base: impl Into<String>) -> Result<Self> {
        Ok(Self::with_client(
            build_client(Some(Duration::from_secs(30)))?,
            base,
        ))
    }

    pub fn with_client(http: reqwest::Client, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into(),
        }
    }

    pub async fn nickname(&self, uid: &str) -> Result<String> {
        let url = format!("{}/user/wapi/getUserFullInfo?uid={uid}", self.base);
        let data: UserInfoData = self.get_json(&url).await?;
        Ok(data.user_info.nickname)
    }

    pub async fn page(&self, uid: &str, offset: Option<&str>) -> Result<Page> {
        let mut url = format!(
            "{}/painter/wapi/userPostList?uid={uid}&size={PAGE_SIZE}",
            self.base
        );
        if let Some(offset) = offset {
            url.push_str(&format!("&offset={offset}"));
        }
        let data: UserPostListData = self.get_json(&url).await?;
        Ok(Page {
            images: data.list.into_iter().flat_map(|i| i.post.images).collect(),
            is_last: data.is_last,
            next_offset: data.next_offset.filter(|o| !o.is_empty()),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut last_err = None;
        for attempt in 1..=RETRIES {
            match self.fetch_once(url).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < RETRIES {
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    }
                }
            }
        }
        bail!(
            "{url} 请求失败: {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )
    }

    async fn fetch_once<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::REFERER, REFERER)
            .send()
            .await?
            .error_for_status()?;
        let body = resp.text().await?;
        let env: Envelope<T> = serde_json::from_str(&body)?;
        match (env.retcode, env.data) {
            (0, Some(data)) => Ok(data),
            (0, None) => bail!("响应缺少 data 字段"),
            (code, _) => bail!("retcode={code}: {}", env.message),
        }
    }
}

/// 从环境变量读取代理地址，优先级：HTTPS_PROXY > HTTP_PROXY > ALL_PROXY（大小写两种形式均可）。
pub fn env_proxy_url() -> Option<String> {
    pick_proxy_url(|key| std::env::var(key).ok())
}

/// 按固定优先级挑选第一个非空的代理地址。
fn pick_proxy_url(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    const KEYS: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    KEYS.iter().find_map(|key| {
        get(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// 构建带 UA、连接超时的 HTTP 客户端。
///
/// 只认代理环境变量：存在则启用，不存在则**显式关闭代理**——否则 reqwest 会
/// 回退到自动系统代理（`auto_sys_proxy`），在 Windows 上读注册表
/// `Internet Settings` 的系统代理设置。
pub fn build_client(timeout: Option<Duration>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60));
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    match env_proxy_url() {
        Some(url) => {
            let proxy = reqwest::Proxy::all(&url)
                .with_context(|| format!("代理地址无效: {url}"))?
                .no_proxy(reqwest::NoProxy::from_env());
            builder = builder.proxy(proxy);
        }
        None => builder = builder.no_proxy(),
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Resp, base_url, spawn};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn api_at(base: String) -> Api {
        Api::with_client(client(), base)
    }

    #[test]
    fn proxy_priority_https_then_http_then_all() {
        let values: HashMap<&str, &str> = HashMap::from([
            ("HTTPS_PROXY", "http://a:1"),
            ("HTTP_PROXY", "http://b:2"),
            ("ALL_PROXY", "http://c:3"),
        ]);
        let picked = pick_proxy_url(|key| values.get(key).map(|v| v.to_string()));
        assert_eq!(picked.as_deref(), Some("http://a:1"));
    }

    #[test]
    fn proxy_skips_empty_values() {
        let values: HashMap<&str, &str> =
            HashMap::from([("HTTPS_PROXY", "   "), ("ALL_PROXY", "http://c:3")]);
        let picked = pick_proxy_url(|key| values.get(key).map(|v| v.to_string()));
        assert_eq!(picked.as_deref(), Some("http://c:3"));
    }

    #[test]
    fn proxy_none_when_absent() {
        assert_eq!(pick_proxy_url(|_| None), None);
    }

    #[tokio::test]
    async fn nickname_is_parsed_from_response() {
        let addr = spawn(Arc::new(|path: &str| {
            assert!(path.starts_with("/user/wapi/getUserFullInfo?uid=76438443"));
            Resp::json(
                r#"{"retcode":0,"message":"OK","data":{"user_info":{"nickname":"测试用户"}}}"#,
            )
        }))
        .await;

        let nick = api_at(base_url(addr)).nickname("76438443").await.unwrap();
        assert_eq!(nick, "测试用户");
    }

    #[tokio::test]
    async fn page_flattens_images_and_reports_offset() {
        let addr = spawn(Arc::new(|_| {
            Resp::json(
                r#"{"retcode":0,"message":"OK","data":{
                    "is_last":false,
                    "next_offset":"20",
                    "list":[
                        {"post":{"images":["http://img/1.jpg","http://img/2.jpg"]}},
                        {"post":{"images":[]}},
                        {"post":{"images":["http://img/3.jpg"]}}
                    ]}}"#,
            )
        }))
        .await;

        let page = api_at(base_url(addr)).page("1", None).await.unwrap();
        assert_eq!(
            page.images,
            ["http://img/1.jpg", "http://img/2.jpg", "http://img/3.jpg"]
        );
        assert!(!page.is_last);
        assert_eq!(page.next_offset.as_deref(), Some("20"));
    }

    #[tokio::test]
    async fn page_treats_missing_offset_as_end() {
        let addr = spawn(Arc::new(|_| {
            Resp::json(r#"{"retcode":0,"message":"OK","data":{"is_last":true,"list":[]}}"#)
        }))
        .await;

        let page = api_at(base_url(addr)).page("1", None).await.unwrap();
        assert!(page.is_last);
        assert_eq!(page.next_offset, None);
        assert!(page.images.is_empty());
    }

    #[tokio::test]
    async fn page_retries_after_transient_failure() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let addr = spawn(Arc::new(move |_| {
            if hits_in_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                Resp::status(500)
            } else {
                Resp::json(r#"{"retcode":0,"message":"OK","data":{"is_last":true,"list":[]}}"#)
            }
        }))
        .await;

        let page = api_at(base_url(addr)).page("1", None).await.unwrap();
        assert!(page.is_last);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn nonzero_retcode_is_reported() {
        let addr = spawn(Arc::new(|_| {
            Resp::json(r#"{"retcode":1001,"message":"内容不可见","data":null}"#)
        }))
        .await;

        let err = api_at(base_url(addr))
            .page("1", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("retcode=1001"), "{err}");
        assert!(err.contains("内容不可见"), "{err}");
    }

    #[tokio::test]
    async fn http_error_is_reported() {
        let addr = spawn(Arc::new(|_| Resp::status(404))).await;

        let err = api_at(base_url(addr))
            .nickname("1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("请求失败"), "{err}");
    }
}
