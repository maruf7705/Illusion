use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub stream_url: String,
    pub audio_url: Option<String>,
    pub filename: String,
    pub title: String,
    pub total_size: Option<u64>,
    pub headers: HashMap<String, String>,
    pub user_agent: Option<String>,
}

pub fn is_video_page_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    (lower.contains("youtube.com/") && !lower.contains("/videoplayback"))
        || lower.contains("youtu.be/")
        || (lower.contains("facebook.com/") && !lower.contains("fbcdn.net"))
        || lower.contains("fb.watch/")
        || lower.contains("instagram.com/")
        || lower.contains("twitter.com/")
        || lower.contains("x.com/")
        || lower.contains("pinterest.com/")
        || (lower.contains("tiktok.com/") && !lower.contains("tiktokcdn.com"))
        || (lower.contains("vimeo.com/") && !lower.contains("vimeocdn.com"))
        || lower.contains("dailymotion.com/")
        || lower.contains("dai.ly/")
        || lower.contains("vk.com/")
        || lower.contains("reddit.com/")
}

pub fn is_media_stream_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("googlevideo.com/videoplayback")
        || lower.contains("fbcdn.net")
        || lower.contains("cdninstagram.com")
        || lower.contains("tiktokcdn.com")
        || lower.contains("byteoversea.com")
        || lower.contains("vimeocdn.com")
        || lower.contains("video.twimg.com")
        || lower.contains(".m3u8")
        || lower.contains(".mpd")
        || lower.contains(".mp4")
        || lower.contains(".webm")
        || lower.contains(".mkv")
        || lower.contains(".ts")
}

fn unescape_json_url(raw: &str) -> String {
    let mut s = raw.replace("\\/", "/");
    s = s.replace("\\u0026", "&");
    s = s.replace("\\u002F", "/");
    s = s.replace("&amp;", "&");
    s
}

fn extract_title_from_html(html: &str, default_title: &str) -> String {
    if let Some(caps) = html.lines().find_map(|line| {
        if line.contains("property=\"og:title\"") || line.contains("name=\"twitter:title\"") {
            let start = line.find("content=\"")?;
            let rest = &line[start + 9..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        } else {
            None
        }
    }) {
        caps
    } else if let Some(start) = html.find("<title>") {
        let rest = &html[start + 7..];
        if let Some(end) = rest.find("</title>") {
            rest[..end].replace(" - YouTube", "").trim().to_string()
        } else {
            default_title.to_string()
        }
    } else {
        default_title.to_string()
    }
}

pub fn extract_facebook_media(html: &str, _page_url: &str) -> Option<ResolvedMedia> {
    // IDM AVFacebookParser equivalent:
    // Look for progressive URLs first (contains muxed audio+video)
    let re_hd = Regex::new(r#""playable_url_quality_hd"\s*:\s*"([^"]+)""#).ok()?;
    let re_sd = Regex::new(r#""playable_url"\s*:\s*"([^"]+)""#).ok()?;
    let re_native_hd = Regex::new(r#""browser_native_hd_url"\s*:\s*"([^"]+)""#).ok()?;
    let re_native_sd = Regex::new(r#""browser_native_sd_url"\s*:\s*"([^"]+)""#).ok()?;
    let re_fbcdn = Regex::new(r#"(https?:\\/\\/[^"]+?\.fbcdn\.net\\/[^"]+?\.mp4[^"]*)"#).ok()?;

    let target_url = if let Some(cap) = re_hd.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_native_hd.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_sd.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_native_sd.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_fbcdn.captures(html) {
        unescape_json_url(&cap[1])
    } else {
        return None;
    };

    let title = extract_title_from_html(html, "facebook_video");
    let clean_title = crate::prober::sanitize_filename(&title);

    let mut headers = HashMap::new();
    headers.insert("Referer".to_string(), "https://www.facebook.com/".to_string());
    headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string());

    Some(ResolvedMedia {
        stream_url: target_url,
        audio_url: None,
        filename: format!("{}.mp4", clean_title),
        title,
        total_size: None,
        headers,
        user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
    })
}

pub fn extract_instagram_media(html: &str, _page_url: &str) -> Option<ResolvedMedia> {
    // IDM AVInstagramParser equivalent:
    let re_video_url = Regex::new(r#""video_url"\s*:\s*"([^"]+)""#).ok()?;
    let re_video_ver = Regex::new(r#""video_versions"\s*:\s*\[\s*\{[^}]*?"url"\s*:\s*"([^"]+)""#).ok()?;
    let re_igcdn = Regex::new(r#"(https?:\\/\\/[^"]+?\.cdninstagram\.com\\/[^"]+?\.mp4[^"]*)"#).ok()?;

    let target_url = if let Some(cap) = re_video_url.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_video_ver.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_igcdn.captures(html) {
        unescape_json_url(&cap[1])
    } else {
        return None;
    };

    let title = extract_title_from_html(html, "instagram_reel");
    let clean_title = crate::prober::sanitize_filename(&title);

    let mut headers = HashMap::new();
    headers.insert("Referer".to_string(), "https://www.instagram.com/".to_string());
    headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string());

    Some(ResolvedMedia {
        stream_url: target_url,
        audio_url: None,
        filename: format!("{}.mp4", clean_title),
        title,
        total_size: None,
        headers,
        user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
    })
}

pub fn extract_tiktok_media(html: &str, _page_url: &str) -> Option<ResolvedMedia> {
    let re_play_addr = Regex::new(r#""playAddr"\s*:\s*"([^"]+)""#).ok()?;
    let re_download_addr = Regex::new(r#""downloadAddr"\s*:\s*"([^"]+)""#).ok()?;
    let re_ttcdn = Regex::new(r#"(https?:\\/\\/[^"]+?(?:tiktokcdn|byteoversea)\.com\\/[^"]+?\.mp4[^"]*)"#).ok()?;

    let target_url = if let Some(cap) = re_play_addr.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_download_addr.captures(html) {
        unescape_json_url(&cap[1])
    } else if let Some(cap) = re_ttcdn.captures(html) {
        unescape_json_url(&cap[1])
    } else {
        return None;
    };

    let title = extract_title_from_html(html, "tiktok_video");
    let clean_title = crate::prober::sanitize_filename(&title);

    let mut headers = HashMap::new();
    headers.insert("Referer".to_string(), "https://www.tiktok.com/".to_string());
    headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string());

    Some(ResolvedMedia {
        stream_url: target_url,
        audio_url: None,
        filename: format!("{}.mp4", clean_title),
        title,
        total_size: None,
        headers,
        user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
    })
}

pub fn extract_twitter_media(html: &str, _page_url: &str) -> Option<ResolvedMedia> {
    let re_twimg = Regex::new(r#"(https?://video\.twimg\.com/[^"]+?\.mp4(?:[?][^"]*)?)"#).ok()?;

    let target_url = if let Some(cap) = re_twimg.captures(html) {
        cap[1].to_string()
    } else {
        return None;
    };

    let title = extract_title_from_html(html, "twitter_video");
    let clean_title = crate::prober::sanitize_filename(&title);

    let mut headers = HashMap::new();
    headers.insert("Referer".to_string(), "https://x.com/".to_string());
    headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string());

    Some(ResolvedMedia {
        stream_url: target_url,
        audio_url: None,
        filename: format!("{}.mp4", clean_title),
        title,
        total_size: None,
        headers,
        user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
    })
}

pub async fn resolve_media_url(
    page_url: &str,
    _preferred_format: Option<&str>,
) -> Result<ResolvedMedia> {
    info!("Resolving media stream natively (Pure Native / Zero External Tools) for: {}", page_url);

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // 1. If it's already a direct media stream URL (e.g. from browser extension or direct link)
    if is_media_stream_url(page_url) {
        let parsed_url = url::Url::parse(page_url)?;
        let title = parsed_url
            .query_pairs()
            .find(|(k, _)| k == "title" || k == "filename")
            .map(|(_, v)| v.to_string())
            .unwrap_or_else(|| {
                parsed_url
                    .path_segments()
                    .and_then(|s| s.last())
                    .unwrap_or("media_stream")
                    .to_string()
            });

        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string());
        if let Some(host) = parsed_url.host_str() {
            headers.insert("Host".to_string(), host.to_string());
        }

        let clean_filename = crate::prober::sanitize_filename(&title);
        return Ok(ResolvedMedia {
            stream_url: page_url.to_string(),
            audio_url: None,
            filename: if clean_filename.contains('.') { clean_filename } else { format!("{}.mp4", clean_filename) },
            title,
            total_size: None,
            headers,
            user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
        });
    }

    // 2. If it is a YouTube watch URL, resolve natively via pure Rust InnerTube & base.js decipherer
    if page_url.contains("youtube.com") || page_url.contains("youtu.be") {
        match crate::innertube::resolve_youtube_media(page_url).await {
            Ok(media) => return Ok(media),
            Err(e) => {
                tracing::warn!("Native InnerTube resolver encountered issue: {}. Falling back to OpenGraph page parsing.", e);
            }
        }
    }

    // 3. Fetch HTML page to inspect OpenGraph tags, HTML5 video tags, or embedded Relay/GraphQL metadata
    let resp = client.get(page_url).send().await?;
    let html = resp.text().await?;

    // 4. Try specialized social media parsers (equivalent to IDM AVFacebookParser, AVInstagramParser, etc.)
    if page_url.contains("facebook.com") || page_url.contains("fb.watch") {
        if let Some(media) = extract_facebook_media(&html, page_url) {
            return Ok(media);
        }
    }

    if page_url.contains("instagram.com") {
        if let Some(media) = extract_instagram_media(&html, page_url) {
            return Ok(media);
        }
    }

    if page_url.contains("tiktok.com") {
        if let Some(media) = extract_tiktok_media(&html, page_url) {
            return Ok(media);
        }
    }

    if page_url.contains("twitter.com") || page_url.contains("x.com") {
        if let Some(media) = extract_twitter_media(&html, page_url) {
            return Ok(media);
        }
    }

    // Extract title from og:title or <title>
    let title = extract_title_from_html(&html, "video");

    // Check OpenGraph video stream URL
    let mut stream_url: Option<String> = None;
    for line in html.lines() {
        if (line.contains("property=\"og:video:secure_url\"")
            || line.contains("property=\"og:video:url\"")
            || line.contains("property=\"og:video\""))
            && line.contains("content=\"http")
        {
            if let Some(start) = line.find("content=\"") {
                let rest = &line[start + 9..];
                if let Some(end) = rest.find('"') {
                    let candidate = &rest[..end];
                    if candidate.contains(".mp4") || candidate.contains(".m3u8") || candidate.contains("fbcdn.net") {
                        stream_url = Some(candidate.to_string());
                        break;
                    }
                }
            }
        }
    }

    // Check YouTube ytInitialPlayerResponse in pure Rust fallback
    if stream_url.is_none() && (page_url.contains("youtube.com") || page_url.contains("youtu.be")) {
        if let Some(start) = html.find("ytInitialPlayerResponse = {") {
            let rest = &html[start + 26..];
            if let Some(end) = rest.find("};") {
                let json_slice = &rest[..=end];
                if let Ok(v) = serde_json::from_str::<Value>(json_slice) {
                    if let Some(fmts) = v.pointer("/streamingData/formats").and_then(|f| f.as_array()) {
                        for fmt in fmts {
                            if let Some(u) = fmt.get("url").and_then(|u| u.as_str()) {
                                stream_url = Some(u.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(url) = stream_url {
        let clean_title = crate::prober::sanitize_filename(&title);
        let ext = if url.contains(".m3u8") { "mp4" } else { "mp4" };
        let filename = format!("{}.{}", clean_title.trim_matches('_'), ext);

        let mut headers = HashMap::new();
        headers.insert("Referer".to_string(), page_url.to_string());
        headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string());

        return Ok(ResolvedMedia {
            stream_url: url,
            audio_url: None,
            filename,
            title,
            total_size: None,
            headers,
            user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
        });
    }

    Err(anyhow!(
        "Protected streaming video on '{}' requires active browser playback session.\n\
         👉 Like Internet Download Manager (IDM), open this video in your browser with the Illusion Browser Extension installed.\n\
         Illusion will intercept the live authenticated video stream, cookies, and tokens, and download natively with 8 connections.",
        page_url
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_facebook_media() {
        let html_mock = r#"
        <html>
        <head><title>Viral Cat Video - Facebook</title></head>
        <body>
        <script>
        {"videoData":[{"playable_url_quality_hd":"https:\/\/video.xx.fbcdn.net\/v\/t42.1797-2\/1000_hd.mp4?_nc_cat=101\u0026efg=eyJ2ZW5jb2RlX3RhZyI6ImRhc2hfaGQifQ"}]}
        </script>
        </body></html>
        "#;
        let res = extract_facebook_media(html_mock, "https://www.facebook.com/reel/123456").unwrap();
        assert!(res.stream_url.starts_with("https://video.xx.fbcdn.net/v/t42.1797-2/1000_hd.mp4?_nc_cat=101&efg="));
        assert_eq!(res.title, "Viral Cat Video - Facebook");
    }

    #[test]
    fn test_extract_instagram_media() {
        let html_mock = r#"
        <html>
        <head><meta property="og:title" content="Nature Reel on Instagram"></head>
        <body>
        <script>
        {"entry_data":{"PostPage":[{"graphql":{"shortcode_media":{"video_url":"https:\/\/scontent.cdninstagram.com\/v\/t50.2886-16\/300_n.mp4?_nc_ht=scontent.cdninstagram.com\u0026_nc_cat=105"}}}]}}
        </script>
        </body></html>
        "#;
        let res = extract_instagram_media(html_mock, "https://www.instagram.com/reel/abcdef/").unwrap();
        assert!(res.stream_url.starts_with("https://scontent.cdninstagram.com/v/t50.2886-16/300_n.mp4?_nc_ht="));
        assert_eq!(res.title, "Nature Reel on Instagram");
    }

    #[test]
    fn test_extract_tiktok_media() {
        let html_mock = r#"
        <html>
        <head><title>Funny Dance | TikTok</title></head>
        <body>
        <script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">
        {"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"video":{"playAddr":"https:\/\/v16-webapp-prime.tiktokcdn.com\/video\/tos\/alisg\/tos-alisg-pve-0037c001\/abc.mp4?a=1988\u0026bti=ODszNWYuMDEj"}}}}}
        </script>
        </body></html>
        "#;
        let res = extract_tiktok_media(html_mock, "https://www.tiktok.com/@user/video/12345").unwrap();
        assert!(res.stream_url.starts_with("https://v16-webapp-prime.tiktokcdn.com/video/tos/alisg/tos-alisg-pve-0037c001/abc.mp4?a=1988&bti="));
        assert_eq!(res.title, "Funny Dance | TikTok");
    }

    #[test]
    fn test_extract_twitter_media() {
        let html_mock = r#"
        <html>
        <head><meta name="twitter:title" content="Breaking News clip"></head>
        <body>
        <a href="https://video.twimg.com/ext_tw_video/123456789/pu/vid/1280x720/vid.mp4?tag=12">Video</a>
        </body></html>
        "#;
        let res = extract_twitter_media(html_mock, "https://x.com/user/status/123456789").unwrap();
        assert_eq!(res.stream_url, "https://video.twimg.com/ext_tw_video/123456789/pu/vid/1280x720/vid.mp4?tag=12");
        assert_eq!(res.title, "Breaking News clip");
    }
}
