#![allow(unused_imports, unused_variables, dead_code)]
//! 下载计划准备与元数据搜索。

#[cfg(feature = "official-api")]
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tracing::{info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::base_system::json_extract;
use crate::network_parser::network::{FanqieWebConfig, FanqieWebNetwork};

use super::models::{
    BookMeta, ChapterRange, ChapterRef, DownloadPlan, merge_meta_prefer_hint_name,
};
#[cfg(feature = "official-api")]
use super::models::{drop_tag_equals_category, merge_meta, merge_tag_lists};
#[cfg(feature = "official-api")]
use super::third_party::resolve_api_urls;

#[cfg(feature = "official-api")]
use tomato_novel_official_api::{DirectoryClient, SearchClient};

// ── 下载计划准备（官方 API 版本）──────────────────────────────────

#[cfg(feature = "official-api")]
pub fn prepare_download_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备下载计划");
    let directory = DirectoryClient::new().context("init DirectoryClient")?;
    let (dir_url, _content_urls) = resolve_api_urls(config)?;
    let api_url = dir_url.as_deref();

    let web_plan = prepare_download_plan_web(config, book_id, meta_hint.clone()).ok();

    let mut dir = match directory.fetch_directory_with_cover(book_id, api_url, None) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "download", book_id, error = %e, "官方 API 获取目录失败（短篇无目录报错属正常现象），将自动采用网页抓取模式进行正文获取");
            if let Some(plan) = web_plan { return Ok(plan); }
            return Err(anyhow!(e).context(format!("fetch directory for book_id={book_id}")));
        }
    };

    if dir.chapters.is_empty() {
        warn!(target: "download", book_id, "官方 API 目录为空，尝试使用 web 回退");
        if let Some(plan) = web_plan { return Ok(plan); }
        return Err(anyhow!("目录为空"));
    }

    let meta_from_dir: BookMeta = dir.meta.clone().into();
    let merged = merge_meta_prefer_hint_name(meta_from_dir, meta_hint);
    let mut completed_meta = if merged.book_name.is_some() && merged.author.is_some() && merged.description.is_some() {
        merged
    } else {
        merge_meta(merged, search_metadata(book_id).unwrap_or_default())
    };

    if let Some(web_plan) = web_plan.as_ref() {
        completed_meta = merge_meta(completed_meta, web_plan.meta.clone());
        completed_meta.tags = merge_tag_lists(&completed_meta.tags, &web_plan.meta.tags);
        completed_meta.tags = drop_tag_equals_category(&completed_meta.tags, &completed_meta.category);
        completed_meta.finished = web_plan.meta.finished.or(completed_meta.finished);
    }

    if let Some(preferred_name) = config.pick_preferred_book_name(&completed_meta) {
        completed_meta.book_name = Some(preferred_name);
    }

    {
        let cover_dir = book_paths::book_folder_path(config, book_id, completed_meta.book_name.as_deref());
        download_web_cover(config, book_id, &completed_meta, &cover_dir);
    }

    if let Some(web_plan) = web_plan.as_ref() {
        dir.chapters = merge_chapters_with_web(dir.chapters, &web_plan.chapters);
    }

    Ok(DownloadPlan {
        book_id: dir.book_id.clone(),
        meta: completed_meta,
        chapters: dir.chapters,
        _raw: dir.raw,
    })
}

#[cfg(not(feature = "official-api"))]
pub fn prepare_download_plan(config: &Config, book_id: &str, meta_hint: BookMeta) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备下载计划（no-official）");
    prepare_download_plan_web(config, book_id, meta_hint)
}

fn parse_chapter_ref_from_value(v: &Value) -> Option<ChapterRef> {
    let maps = json_extract::collect_maps(v);
    let id = maps.iter().find_map(|m| json_extract::pick_string(m, &["item_id", "itemId", "chapter_id", "chapterId", "catalog_id", "catalogId", "id"]))?;
    let title = maps.iter().find_map(|m| json_extract::pick_string(m, &["title", "chapter_title", "chapterTitle", "name", "chapter_name"])).unwrap_or_else(|| id.clone());
    Some(ChapterRef { id, title })
}

fn prepare_download_plan_web(config: &Config, book_id: &str, meta_hint: BookMeta) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备下载计划（web fallback）");

    let web_cfg = FanqieWebConfig {
        request_timeout: Duration::from_secs(config.request_timeout.max(1)),
        max_retries: config.max_retries.max(1) as usize,
        ..Default::default()
    };
    let web = FanqieWebNetwork::new(web_cfg).context("init FanqieWebNetwork")?;

    // 依然是拿这 9 个老老实实的参数，别的代码完全不会崩
    let (
        book_name,
        author,
        description,
        tags_opt,
        cover_url,
        detail_cover_url,
        _html_img_cover_url,
        chapter_count,
        finished,
    ) = web.get_book_info(book_id);

    // 🌟 这里是重点！去内部背包里拿隐藏的 ID
    let first_item_id = web.last_first_item_id.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let chapter_values = web.fetch_chapter_list(book_id);

    let mut chapters: Vec<ChapterRef> = Vec::new();
    let is_short_story: bool;

    match chapter_values {
        Some(ref values) if !values.is_empty() => {
            is_short_story = false;
            chapters = values.iter().filter_map(parse_chapter_ref_from_value).collect();
            if chapters.is_empty() { return Err(anyhow!("解析章节列表失败（未能提取 item_id/title）")); }
        }
        _ => {
            info!(target: "download", book_id, "确认为短篇小说，自动提取隐藏正文 ID 并构建单章结构");
            is_short_story = true;
            // 🌟 直接应用背包里拿出来的短篇 ID！
            let real_post_id = first_item_id.unwrap_or_else(|| book_id.to_string());
            chapters.push(ChapterRef {
                id: real_post_id,
                title: book_name.clone().unwrap_or_default(),
            });
        }
    }

    let web_meta = BookMeta {
        book_name, author, description, tags: tags_opt.unwrap_or_default(), cover_url, detail_cover_url, chapter_count, finished, ..BookMeta::default()
    };

    let mut completed_meta = merge_meta_prefer_hint_name(web_meta, meta_hint);
    if let Some(preferred_name) = config.pick_preferred_book_name(&completed_meta) { completed_meta.book_name = Some(preferred_name); }

    {
        let cover_dir = book_paths::book_folder_path(config, book_id, completed_meta.book_name.as_deref());
        download_web_cover(config, book_id, &completed_meta, &cover_dir);
    }

    let raw = if is_short_story {
        serde_json::json!({ "book_id": book_id, "chapters": [{ "item_id": chapters[0].id, "title": chapters[0].title, }], "source": "fanqie_web_short_story", })
    } else {
        serde_json::json!({ "book_id": book_id, "chapters": chapter_values, "source": "fanqie_web", })
    };

    Ok(DownloadPlan {
        book_id: book_id.to_string(), meta: completed_meta, chapters: std::mem::take(&mut chapters), _raw: raw,
    })
}

#[cfg(feature = "official-api")]
pub(crate) fn merge_chapters_with_web(official: Vec<ChapterRef>, web: &[ChapterRef]) -> Vec<ChapterRef> {
    if official.is_empty() { return web.to_vec(); }
    if web.is_empty() { return official; }

    let mut web_title_map = HashMap::new();
    for ch in web { if !ch.id.trim().is_empty() { web_title_map.insert(ch.id.clone(), ch.title.clone()); } }

    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(official.len() + web.len().saturating_sub(official.len()));

    for mut ch in official {
        if !seen.insert(ch.id.clone()) { continue; }
        if ch.title.trim().is_empty() && let Some(title) = web_title_map.get(&ch.id) { ch.title = title.clone(); }
        merged.push(ch);
    }
    for ch in web { if !seen.contains(&ch.id) { merged.push(ch.clone()); } }
    merged
}

#[cfg(feature = "official-api")]
fn search_metadata(book_id: &str) -> Option<BookMeta> {
    let client = SearchClient::new().ok()?;
    let resp = client.search_books(book_id).ok()?;
    let book = resp.books.into_iter().find(|b| b.book_id == book_id)?;
    let maps = json_extract::collect_maps(&book.raw);

    let description = maps.iter().find_map(|m| json_extract::pick_string(m, &["description", "desc", "abstract", "intro", "summary"]));
    let tags = maps.iter().find_map(|m| json_extract::pick_tags_opt(m)).unwrap_or_default();
    let cover_url = maps.iter().find_map(|m| json_extract::pick_cover(m));
    let detail_cover_url = maps.iter().find_map(|m| json_extract::pick_detail_cover(m));
    let finished = maps.iter().find_map(|m| json_extract::pick_finished(m));
    let chapter_count = maps.iter().find_map(|m| json_extract::pick_chapter_count(m));
    
    Some(BookMeta {
        book_name: book.title, author: book.author, description, tags, cover_url, detail_cover_url, finished, chapter_count,
        ..BookMeta::default()
    })
}

pub(crate) fn apply_range(chapters: &[ChapterRef], range: Option<ChapterRange>) -> Vec<ChapterRef> {
    let total = chapters.len();
    match range {
        None => chapters.to_vec(),
        Some(r) => {
            if r.start == 0 || r.start > r.end { return Vec::new(); }
            let start_idx = r.start.saturating_sub(1);
            let end_idx = r.end.min(total).saturating_sub(1);
            if start_idx >= chapters.len() { return Vec::new(); }
            chapters.iter().skip(start_idx).take(end_idx.saturating_sub(start_idx) + 1).cloned().collect()
        }
    }
}

fn download_web_cover(config: &Config, book_id: &str, meta: &BookMeta, cover_dir: &std::path::Path) {
    let book_name = meta.book_name.as_deref();
    // 修复警告：加了下划线
    if let Some(_existing) = book_paths::migrate_legacy_cover_file(cover_dir, book_name) { return; }
    let _ = std::fs::create_dir_all(cover_dir);

    let web_cfg = FanqieWebConfig { request_timeout: Duration::from_secs(config.request_timeout.max(1)), max_retries: 2, ..Default::default() };
    let web = match FanqieWebNetwork::new(web_cfg) { Ok(w) => w, Err(_) => return, };
    
    let (_, _, _, _, _, _, html_img_cover_url, _, _) = web.get_book_info(book_id);
    let img_url = match html_img_cover_url { Some(ref u) if !u.trim().is_empty() => u.as_str(), _ => return, };

    let timeout = Duration::from_millis(10_000);
    let max_retries = 3u32;
    for attempt in 0..max_retries {
        if attempt > 0 { std::thread::sleep(Duration::from_millis(300u64 * (1u64 << attempt.min(3)))); }
        let bytes = match crate::third_party::media_fetch::fetch_bytes(img_url, timeout) { Some(b) if !b.is_empty() => b, _ => continue, };
        if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"heic" | b"heix" | b"mif1" | b"msf1") { return; }

        let ext = if bytes.len() >= 8 && bytes[0] == 0x89 && &bytes[1..4] == b"PNG" { "png" } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" { "webp" } else { "jpg" };
        let path = book_paths::canonical_cover_path(cover_dir, ext);
        if std::fs::write(&path, &bytes).is_ok() { return; }
    }
}
