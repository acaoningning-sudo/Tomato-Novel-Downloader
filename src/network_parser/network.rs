#![allow(dead_code)]

use reqwest::blocking::Client;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONNECTION, CONTENT_TYPE, HeaderMap, HeaderValue, REFERER, USER_AGENT,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, warn};

// 编译一次复用的正则缓存
fn re_next_data() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r#"(?s)<script[^>]*id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).unwrap()
    })
}

fn re_initial_state() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r#"(?s)window\.__INITIAL_STATE__\s*=\s*(\{.*?\})\s*;"#).unwrap()
    })
}

fn re_info_label_grey() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r#"<span[^>]*class="info-label-grey"[^>]*>([^<]+)</span>"#).unwrap()
    })
}

fn re_info_label_yellow() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r#"<span[^>]*class="info-label-yellow"[^>]*>([^<]+)</span>"#).unwrap()
    })
}

fn re_ld_json() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r#"<script[^>]*type="application/ld\+json"[^>]*>\s*([\s\S]*?)\s*</script>"#,
        )
        .unwrap()
    })
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BookInfo {
    pub book_name: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub cover_url: Option<String>,
    pub detail_cover_url: Option<String>,
    pub html_img_cover_url: Option<String>,
    pub chapter_count: Option<usize>,
    pub finished: Option<bool>,
    // 🌟新增核心：抓取真实的短篇正文 ID
    pub first_item_id: Option<String>, 
}

#[derive(Debug, Clone)]
pub(crate) struct FanqieWebConfig {
    pub request_timeout: Duration,
    pub max_retries: usize,
    pub insecure_tls: bool,
    pub user_agent: String,
    pub cache_dir: PathBuf,
}

impl Default for FanqieWebConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(15),
            max_retries: 3,
            insecure_tls: false,
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36".to_string(),
            cache_dir: std::env::temp_dir().join("tomato-novel-downloader").join("dir_cache"),
        }
    }
}

pub(crate) struct FanqieWebNetwork {
    client: Client,
    config: FanqieWebConfig,
    last_dir_fetch: Mutex<Instant>,
}

pub(crate) type BookInfoParts = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<String>>,
    Option<String>,
    Option<String>,
    Option<String>, 
    Option<usize>,
    Option<bool>,
    Option<String>, // 🌟新增的返回值位置
);

impl FanqieWebNetwork {
    pub(crate) fn new(config: FanqieWebConfig) -> anyhow::Result<Self> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        default_headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));

        let client = Client::builder()
            .default_headers(default_headers)
            .danger_accept_invalid_certs(config.insecure_tls)
            .timeout(config.request_timeout)
            .build()?;

        Ok(Self {
            client,
            config,
            last_dir_fetch: Mutex::new(Instant::now() - Duration::from_secs(60)),
        })
    }

    fn get_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.config.user_agent)
                .unwrap_or(HeaderValue::from_static("Mozilla/5.0")),
        );
        headers
    }

    fn get_json_headers(&self, book_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/plain, */*"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.config.user_agent)
                .unwrap_or(HeaderValue::from_static("Mozilla/5.0")),
        );
        let referer = format!("https://fanqienovel.com/page/{book_id}");
        if let Ok(v) = HeaderValue::from_str(&referer) {
            headers.insert(REFERER, v);
        }
        headers
    }

    pub(crate) fn get_book_info(&self, book_id: &str) -> BookInfoParts {
        let book_info_url = format!("https://fanqienovel.com/page/{book_id}");

        match self
            .client
            .get(&book_info_url)
            .headers(self.get_headers())
            .send()
        {
            Ok(resp) => {
                if resp.status().as_u16() == 404 {
                    warn!("小说ID {} 主页 404，尝试回退至阅读页获取信息...", book_id);
                    let reader_url = format!("https://fanqienovel.com/reader/{}", book_id);
                    if let Ok(reader_resp) = self.client.get(&reader_url).headers(self.get_headers()).send() {
                        if reader_resp.status().is_success() {
                            if let Ok(text) = reader_resp.text() {
                                let info = ContentParser::parse_book_info(&text, book_id);
                                return (
                                    info.book_name,
                                    info.author,
                                    info.description,
                                    info.tags,
                                    info.cover_url,
                                    info.detail_cover_url,
                                    info.html_img_cover_url,
                                    info.chapter_count,
                                    info.finished,
                                    info.first_item_id,
                                );
                            }
                        }
                    }
                    error!("小说ID {} 不存在！", book_id);
                    return (None, None, None, None, None, None, None, None, None, None);
                }

                let resp = match resp.error_for_status() {
                    Ok(r) => r,
                    Err(e) => {
                        error!("获取书籍信息失败: {}", e);
                        return (None, None, None, None, None, None, None, None, None, None);
                    }
                };

                match resp.text() {
                    Ok(text) => {
                        let info = ContentParser::parse_book_info(&text, book_id);
                        (
                            info.book_name,
                            info.author,
                            info.description,
                            info.tags,
                            info.cover_url,
                            info.detail_cover_url,
                            info.html_img_cover_url,
                            info.chapter_count,
                            info.finished,
                            info.first_item_id,
                        )
                    }
                    Err(e) => {
                        error!("获取书籍信息失败: {}", e);
                        (None, None, None, None, None, None, None, None, None, None)
                    }
                }
            }
            Err(e) => {
                error!("获取书籍信息失败: {}", e);
                (None, None, None, None, None, None, None, None, None, None)
            }
        }
    }

    pub(crate) fn fetch_chapter_list(&self, book_id: &str) -> Option<Vec<Value>> {
        if book_id.trim().is_empty() || !book_id.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }

        let api_url = format!("https://fanqienovel.com/api/reader/directory/detail?bookId={book_id}");
        self.throttle_directory(Duration::from_millis(800));

        let retries = self.config.max_retries.max(1);
        let mut backoff = 0.6f64;
        
        // 修正：加了下划线前缀，消除 unused variable 警告
        let mut _last_error: Option<String> = None;

        for attempt in 1..=retries {
            let headers = self.get_json_headers(book_id);
            let resp = self.client.get(&api_url).headers(headers).send();

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    _last_error = Some(e.to_string());
                    self.sleep_backoff(attempt, retries, &mut backoff, 0.3);
                    continue;
                }
            };

            if resp.status().as_u16() == 403 {
                _last_error = Some("403 Forbidden".to_string());
                if attempt == 1 {
                    let warm_url = format!("https://fanqienovel.com/page/{book_id}");
                    let _ = self.client.get(&warm_url).headers(self.get_headers()).send();
                }
                self.sleep_backoff(attempt, retries, &mut backoff, 0.4);
                continue;
            }

            let resp = match resp.error_for_status() {
                Ok(r) => r,
                Err(e) => {
                    _last_error = Some(e.to_string());
                    self.sleep_backoff(attempt, retries, &mut backoff, 0.3);
                    continue;
                }
            };

            let data: Value = match resp.json() {
                Ok(v) => v,
                Err(e) => {
                    _last_error = Some(e.to_string());
                    self.sleep_backoff(attempt, retries, &mut backoff, 0.3);
                    continue;
                }
            };

            let _ = self.save_dir_cache(book_id, &data);

            if let Some(list) = Self::parse_chapter_data(&data) {
                return Some(list);
            }

            _last_error = Some("parse chapter list failed".to_string());
            self.sleep_backoff(attempt, retries, &mut backoff, 0.3);
            continue;
        }

        match self.load_dir_cache(book_id) {
            Ok(Some(cached)) => Self::parse_chapter_data(&cached),
            _ => None,
        }
    }

    fn throttle_directory(&self, min_gap: Duration) {
        if let Ok(mut last) = self.last_dir_fetch.lock() {
            let elapsed = last.elapsed();
            if elapsed < min_gap {
                std::thread::sleep(min_gap - elapsed);
            }
            *last = Instant::now();
        }
    }

    fn sleep_backoff(&self, attempt: usize, retries: usize, backoff: &mut f64, jitter_max: f64) {
        if attempt >= retries {
            return;
        }
        let jitter = jitter_seconds(jitter_max);
        let sleep_s = (*backoff + jitter).min(3.0);
        std::thread::sleep(Duration::from_millis((sleep_s * 1000.0) as u64));
        *backoff = (*backoff * 2.0).min(3.0);
    }

    fn cache_path(&self, book_id: &str) -> PathBuf {
        self.config.cache_dir.join(format!("{book_id}.json"))
    }

    fn save_dir_cache(&self, book_id: &str, data: &Value) -> anyhow::Result<()> {
        let path = self.cache_path(book_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(data)?;
        fs::write(path, bytes)?;
        Ok(())
    }

    fn load_dir_cache(&self, book_id: &str) -> anyhow::Result<Option<Value>> {
        let path = self.cache_path(book_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        Ok(Some(value))
    }

    fn parse_chapter_data(data: &Value) -> Option<Vec<Value>> {
        let root = data.get("data").unwrap_or(data);
        for key in ["chapterList", "chapter_list", "chapters", "item_list", "items", "list"] {
            if let Some(arr) = root.get(key).and_then(Value::as_array) {
                return Some(arr.clone());
            }
        }
        if let Some(volumes) = root.get("chapterListWithVolume").and_then(Value::as_array) {
            let mut all_chapters: Vec<Value> = Vec::new();
            for vol in volumes {
                if let Some(ch_list) = vol.as_array() {
                    all_chapters.extend(ch_list.iter().cloned());
                }
            }
            if !all_chapters.is_empty() { return Some(all_chapters); }
        }
        if let Some(inner) = root.get("data") {
            for key in ["list", "chapterList", "chapter_list", "items", "item_list", "chapters"] {
                if let Some(arr) = inner.get(key).and_then(Value::as_array) {
                    return Some(arr.clone());
                }
            }
        }
        find_chapter_array(root)
    }
}

fn is_chapter_like_object(map: &serde_json::Map<String, Value>) -> bool {
    let keys = ["item_id", "itemId", "chapter_id", "chapterId", "catalog_id", "catalogId", "id"];
    keys.iter().any(|k| map.contains_key(*k))
}

fn find_chapter_array(value: &Value) -> Option<Vec<Value>> {
    fn walk(value: &Value, best: &mut Option<Vec<Value>>) {
        match value {
            Value::Array(arr) => {
                let is_candidate = arr.iter().any(|v| v.as_object().map(is_chapter_like_object).unwrap_or(false));
                if is_candidate {
                    let replace = match best { Some(existing) => arr.len() > existing.len(), None => true };
                    if replace { *best = Some(arr.clone()); }
                }
                for v in arr { walk(v, best); }
            }
            Value::Object(map) => { for v in map.values() { walk(v, best); } }
            _ => {}
        }
    }
    let mut best: Option<Vec<Value>> = None;
    walk(value, &mut best);
    best
}

fn parse_html_img_cover_url(html: &str) -> Option<String> {
    for caps in re_ld_json().captures_iter(html) {
        let json_text = caps.get(1)?.as_str();
        let Ok(value) = serde_json::from_str::<Value>(json_text) else { continue; };
        for key in ["image", "images"] {
            if let Some(arr) = value.get(key).and_then(Value::as_array)
                && let Some(url) = arr.first().and_then(Value::as_str)
            {
                let u = url.trim();
                if !u.is_empty() && (u.starts_with("http://") || u.starts_with("https://")) { return Some(u.to_string()); }
            }
            if let Some(url) = value.get(key).and_then(Value::as_str) {
                let u = url.trim();
                if !u.is_empty() && (u.starts_with("http://") || u.starts_with("https://")) { return Some(u.to_string()); }
            }
        }
    }
    None
}

struct ContentParser;

impl ContentParser {
    fn parse_book_info(html: &str, _book_id: &str) -> BookInfo {
        let finished_from_label = parse_finished_from_info_label(html);
        let html_img_cover_url = parse_html_img_cover_url(html);

        if let Some(json_text) = extract_next_data_json(html)
            && let Ok(value) = serde_json::from_str::<Value>(&json_text)
        {
            let book_name = find_string_by_key(&value, ["bookName", "book_name", "title", "name"]);
            let author = find_string_by_key(&value, ["author", "authorName", "author_name"]);
            let description = find_string_by_key(&value, ["abstract", "description", "intro", "introduce"]);
            let cover_url = find_string_by_key(&value, ["thumb_url", "expand_thumb_url", "cover_url", "cover", "horiz_thumb_url", "audio_thumb_url_hd"])
                .or_else(|| find_string_by_key(&value, ["thumb_uri"]).and_then(|s| build_cover_url_from_thumb_uri(&s)));
            let detail_cover_url = find_string_by_key(&value, ["detail_page_thumb_url", "detail_cover_url", "detail_thumb_url"]).or_else(|| cover_url.clone());
            let chapter_count = find_usize_by_key(&value, ["chapterCount", "chapter_count"]);
            let tags = find_string_array_by_key(&value, ["tags", "tagNames", "tag_names"]);
            let finished = finished_from_label.or_else(|| find_finished_by_key(&value));
            
            // 🌟 雷达启动：挖掘首章（短篇本体）ID
            let first_item_id = find_string_by_key(&value, ["first_chapter_item_id", "firstChapterItemId", "item_id", "itemId"]);

            if book_name.is_some() || first_item_id.is_some() {
                return BookInfo {
                    book_name, author, description, tags, cover_url, detail_cover_url, html_img_cover_url, chapter_count, finished, first_item_id,
                };
            }
        }

        if let Some(json_text) = extract_initial_state_json(html)
            && let Ok(value) = serde_json::from_str::<Value>(&json_text)
        {
            let book_name = find_string_by_key(&value, ["bookName", "book_name", "title", "name"]);
            let author = find_string_by_key(&value, ["authorName", "author", "author_name"]);
            let description = find_string_by_key(&value, ["abstract", "description", "intro", "introduce"]);
            let cover_url = find_string_by_key(&value, ["thumb_url", "expand_thumb_url", "cover_url", "cover", "horiz_thumb_url", "audio_thumb_url_hd"])
                .or_else(|| find_string_by_key(&value, ["thumb_uri"]).and_then(|s| build_cover_url_from_thumb_uri(&s)));
            let detail_cover_url = find_string_by_key(&value, ["detail_page_thumb_url", "detail_cover_url", "detail_thumb_url"]).or_else(|| cover_url.clone());
            let chapter_count = find_usize_by_key(&value, ["chapterTotal", "chapterCount", "chapter_count"]);
            let tags = find_string_array_by_key(&value, ["tags", "tagNames", "tag_names"]).or_else(|| parse_tags_from_info_label(html));
            let finished = finished_from_label.or_else(|| find_finished_by_key(&value));
            let first_item_id = find_string_by_key(&value, ["first_chapter_item_id", "firstChapterItemId", "item_id", "itemId"]);

            if book_name.is_some() || first_item_id.is_some() {
                return BookInfo {
                    book_name, author, description, tags, cover_url, detail_cover_url, html_img_cover_url, chapter_count, finished, first_item_id,
                };
            }
        }

        let book_name = regex_json_string_field(html, "bookName").or_else(|| regex_json_string_field(html, "book_name"));
        let author = regex_json_string_field(html, "author").or_else(|| regex_json_string_field(html, "authorName"));
        let description = regex_json_string_field(html, "abstract").or_else(|| regex_json_string_field(html, "description"));
        let cover_url = regex_json_string_field(html, "thumb_url").or_else(|| regex_json_string_field(html, "expand_thumb_url")).or_else(|| regex_json_string_field(html, "cover_url")).or_else(|| regex_json_string_field(html, "cover")).or_else(|| regex_json_string_field(html, "horiz_thumb_url")).or_else(|| regex_json_string_field(html, "audio_thumb_url_hd")).or_else(|| regex_json_string_field(html, "thumb_uri").and_then(|s| build_cover_url_from_thumb_uri(&s)));
        let detail_cover_url = regex_json_string_field(html, "detail_page_thumb_url").or_else(|| regex_json_string_field(html, "detail_cover_url")).or_else(|| regex_json_string_field(html, "detail_thumb_url")).or_else(|| cover_url.clone());
        let chapter_count = regex_json_usize_field(html, "chapterCount").or_else(|| regex_json_usize_field(html, "chapter_count"));
        let tags = parse_tags_from_info_label(html);
        let finished = finished_from_label;
        let first_item_id = regex_json_string_field(html, "first_chapter_item_id").or_else(|| regex_json_string_field(html, "firstChapterItemId")).or_else(|| regex_json_string_field(html, "itemId"));

        BookInfo { book_name, author, description, tags, cover_url, detail_cover_url, html_img_cover_url, chapter_count, finished, first_item_id }
    }
}

fn build_cover_url_from_thumb_uri(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() { return None; }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") { return Some(trimmed.to_string()); }
    Some(format!("https://p3-reading-sign.fqnovelpic.com/{}", trimmed))
}

fn extract_next_data_json(html: &str) -> Option<String> {
    let caps = re_next_data().captures(html)?;
    Some(caps.get(1)?.as_str().trim().to_string())
}

fn extract_initial_state_json(html: &str) -> Option<String> {
    let caps = re_initial_state().captures(html)?;
    Some(caps.get(1)?.as_str().trim().to_string())
}

fn find_string_by_key<const N: usize>(value: &Value, keys: [&str; N]) -> Option<String> {
    for key in keys {
        if let Some(s) = find_first_string_for_key(value, key) { return Some(s); }
    }
    None
}

fn find_usize_by_key<const N: usize>(value: &Value, keys: [&str; N]) -> Option<usize> {
    for key in keys {
        if let Some(n) = find_first_usize_for_key(value, key) { return Some(n); }
    }
    None
}

fn find_string_array_by_key<const N: usize>(value: &Value, keys: [&str; N]) -> Option<Vec<String>> {
    for key in keys {
        if let Some(arr) = find_first_string_array_for_key(value, key) { return Some(arr); }
    }
    None
}

fn find_finished_by_key(value: &Value) -> Option<bool> {
    let keys = ["status", "serial_status", "finish_status", "finishStatus", "is_finish", "is_finished"];
    for key in keys {
        if let Some(n) = find_first_i64_for_key(value, key) && let Some(b) = map_status_to_finished(key, n) { return Some(b); }
    }
    None
}

fn map_status_to_finished(key: &str, n: i64) -> Option<bool> {
    match key {
        "status" => match n { 1 => Some(true), 0 => Some(false), 2 => Some(true), _ => None, },
        "serial_status" => match n { 1 => Some(false), 2 => Some(true), _ => None, },
        _ => match n { 1 | 2 => Some(true), 0 => Some(false), _ => None, },
    }
}

fn find_first_string_for_key(value: &Value, target: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(target) && let Some(s) = v.as_str() { return Some(s.to_string()); }
            for v in map.values() { if let Some(found) = find_first_string_for_key(v, target) { return Some(found); } }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_first_string_for_key(v, target)),
        _ => None,
    }
}

fn find_first_usize_for_key(value: &Value, target: &str) -> Option<usize> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(target) {
                if let Some(n) = v.as_u64() { return Some(n as usize); }
                if let Some(s) = v.as_str() && let Ok(n) = s.parse::<usize>() { return Some(n); }
            }
            for v in map.values() { if let Some(found) = find_first_usize_for_key(v, target) { return Some(found); } }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_first_usize_for_key(v, target)),
        _ => None,
    }
}

fn find_first_i64_for_key(value: &Value, target: &str) -> Option<i64> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(target) {
                if let Some(n) = v.as_i64() { return Some(n); }
                if let Some(s) = v.as_str() && let Ok(n) = s.parse::<i64>() { return Some(n); }
            }
            for v in map.values() { if let Some(found) = find_first_i64_for_key(v, target) { return Some(found); } }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_first_i64_for_key(v, target)),
        _ => None,
    }
}

fn find_first_string_array_for_key(value: &Value, target: &str) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(target) && let Some(arr) = v.as_array() {
                let out: Vec<String> = arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                if !out.is_empty() { return Some(out); }
            }
            for v in map.values() { if let Some(found) = find_first_string_array_for_key(v, target) { return Some(found); } }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_first_string_array_for_key(v, target)),
        _ => None,
    }
}

fn regex_json_string_field(html: &str, field: &str) -> Option<String> {
    let pattern = format!(r#"\"{}\"\s*:\s*\"(.*?)\""#, regex::escape(field));
    let re = regex::Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?;
    let raw = caps.get(1)?.as_str();
    let quoted = format!("\"{}\"", raw);
    serde_json::from_str::<String>(&quoted).ok().or_else(|| Some(raw.to_string()))
}

// 🐛 核心修复点：把删漏的 caps 加回来了，这是引发血案的罪魁祸首！
fn regex_json_usize_field(html: &str, field: &str) -> Option<usize> {
    let pattern = format!(r#"\"{}\"\s*:\s*(\d+)"#, regex::escape(field));
    let re = regex::Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?; 
    caps.get(1)?.as_str().parse::<usize>().ok()
}

fn parse_tags_from_info_label(html: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for caps in re_info_label_grey().captures_iter(html) {
        let raw = caps.get(1)?.as_str().trim();
        if !raw.is_empty() { out.push(raw.to_string()); }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn parse_finished_from_info_label(html: &str) -> Option<bool> {
    let caps = re_info_label_yellow().captures(html)?;
    let label = caps.get(1)?.as_str().trim();
    if label.contains("未完结") || label.contains("连载") { return Some(false); }
    if label.contains("完结") { return Some(true); }
    None
}

fn jitter_seconds(max: f64) -> f64 {
    if max <= 0.0 { return 0.0; }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    let bucket = (nanos % 10_000) as f64 / 10_000.0;
    bucket * max
}
