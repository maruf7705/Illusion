use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, REFERER};
use serde_json::Value;
use std::collections::HashMap;
use tracing::info;
use url::Url;

use crate::resolver::ResolvedMedia;

const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub enum DecipherOp {
    Reverse,
    Splice(usize),
    Swap(usize),
}

#[derive(Debug, Clone, Default)]
pub struct Decipherer {
    pub operations: Vec<DecipherOp>,
}

impl Decipherer {
    /// Apply the decipher operations to a scrambled signature string
    pub fn decipher(&self, signature: &str) -> String {
        let mut chars: Vec<char> = signature.chars().collect();
        for op in &self.operations {
            match op {
                DecipherOp::Reverse => chars.reverse(),
                DecipherOp::Splice(n) => {
                    if *n < chars.len() {
                        chars.drain(0..*n);
                    }
                }
                DecipherOp::Swap(n) => {
                    if !chars.is_empty() {
                        let idx = n % chars.len();
                        chars.swap(0, idx);
                    }
                }
            }
        }
        chars.into_iter().collect()
    }

    /// Extract decipher operations from YouTube player base.js
    pub fn parse_from_js(js: &str) -> Option<Self> {
        // 1. Extract w token array: w = "...".split(";")
        let w_tokens = extract_w_tokens(js)?;

        // 2. Identify the operations helper object using IDM Regex 37
        // Object pattern: Obj = { op1: function(a){ ... }, op2: function(a,b){ ... } }
        let (obj_name, reverse_fn, swap_fn, splice_fn) = find_descrambler_object(js, &w_tokens)?;
        info!(
            "Decipherer: Found descrambler object '{}': rev='{}', swap='{}', splice='{}'",
            obj_name, reverse_fn, swap_fn, splice_fn
        );

        // 3. Find decipher calls in JS: e.g. Xn(a, b, sig) or function calling obj_name
        let ops = extract_operations(js, &obj_name, &reverse_fn, &swap_fn, &splice_fn, &w_tokens)?;
        Some(Self { operations: ops })
    }
}

fn extract_w_tokens(js: &str) -> Option<Vec<String>> {
    let re = regex::Regex::new(r#"\bw\s*=\s*"([^"]+)"\s*\.split\(";"\)"#).ok()?;
    if let Some(caps) = re.captures(js) {
        let raw = caps.get(1)?.as_str();
        return Some(raw.split(';').map(|s| s.to_string()).collect());
    }
    None
}

fn find_descrambler_object(
    js: &str,
    w_tokens: &[String],
) -> Option<(String, String, String, String)> {
    // Search for obj definition with reverse, swap, and splice
    // e.g. Wn={b2:function(a){a[w[47]]()}, b6:function(a,b){var c=a[0];a[0]=a[b%a[w[7]]];a[b%a[w[7]]]=c}, aE:function(a,b){a[w[8]](0,b)}}
    let re = regex::Regex::new(r"([\w$]+)\s*=\s*\{([^{}]+:[^{}]+(?:,[^{}]+:[^{}]+)*)\}").ok()?;
    for cap in re.captures_iter(js) {
        let obj_name = cap.get(1)?.as_str();
        let body = cap.get(2)?.as_str();

        let mut rev = String::new();
        let mut swap = String::new();
        let mut splice = String::new();

        for part in body.split(",\n").flat_map(|s| s.split(',')) {
            if let Some(colon) = part.find(':') {
                let fn_name = part[..colon].trim().trim_matches('"');
                let fn_body = &part[colon + 1..];

                if fn_body.contains("reverse")
                    || (fn_body.contains("w[47]") && w_tokens.len() > 47 && w_tokens[47] == "reverse")
                {
                    rev = fn_name.to_string();
                } else if fn_body.contains("c=a[0]") || fn_body.contains("a[0]=a[b%") {
                    swap = fn_name.to_string();
                } else if fn_body.contains("splice")
                    || (fn_body.contains("w[8]") && w_tokens.len() > 8 && w_tokens[8] == "splice")
                {
                    splice = fn_name.to_string();
                }
            }
        }

        if !rev.is_empty() && !swap.is_empty() && !splice.is_empty() {
            return Some((obj_name.to_string(), rev, swap, splice));
        }
    }
    None
}

fn extract_operations(
    js: &str,
    obj_name: &str,
    rev_fn: &str,
    swap_fn: &str,
    splice_fn: &str,
    w: &[String],
) -> Option<Vec<DecipherOp>> {
    // 1. Modern YouTube player pattern with XOR masks:
    // Wn[w[m^6725]](p, m^6657), Wn[w[m^6682]](p, 1), ...
    // and calls like: Xn(68, 6671, ...)
    let xn_call_re = regex::Regex::new(r#"Xn\((\d+)\s*,\s*(\d+)\s*,\s*Xn\((\d+)\s*,\s*(\d+)\s*,\s*[\w$]+\.s\)\)"#).ok()?;
    if let Some(caps) = xn_call_re.captures(js) {
        let a2: usize = caps.get(1)?.as_str().parse().ok()?;
        let b2: usize = caps.get(2)?.as_str().parse().ok()?;
        let m2 = b2 ^ a2;

        let mut ops = Vec::new();

        // Find the function definition for Xn
        let fn_re = regex::Regex::new(r"function\(a,b,c[^{]*\)\{var m=b\^a;(.*?)\breturn\b").ok()?;
        if let Some(fn_caps) = fn_re.captures(js) {
            let body = fn_caps.get(1)?.as_str();

            // Match all Wn[w[m^X]](p, Y) calls in sequence
            let call_re = regex::Regex::new(&format!(
                r#"{}\[w\[m\^(\d+)\]\]\(p(?:\s*,\s*(?:m\^(\d+)|(\d+)))?\)"#,
                regex::escape(obj_name)
            )).ok()?;

            for m in call_re.captures_iter(body) {
                let mask: usize = m.get(1)?.as_str().parse().ok()?;
                let token_idx = m2 ^ mask;
                if token_idx >= w.len() {
                    continue;
                }
                let target_fn = &w[token_idx];

                let arg = if let Some(arg_mask) = m.get(2) {
                    let am: usize = arg_mask.as_str().parse().ok()?;
                    m2 ^ am
                } else if let Some(lit_arg) = m.get(3) {
                    lit_arg.as_str().parse().unwrap_or(1)
                } else {
                    1
                };

                if target_fn == rev_fn || target_fn == "b2" || target_fn == "reverse" {
                    ops.push(DecipherOp::Reverse);
                } else if target_fn == swap_fn || target_fn == "b6" || target_fn == "swap" {
                    ops.push(DecipherOp::Swap(arg));
                } else if target_fn == splice_fn || target_fn == "aE" || target_fn == "splice" {
                    ops.push(DecipherOp::Splice(arg));
                }
            }

            if !ops.is_empty() {
                return Some(ops);
            }
        }
    }

    // 2. Fallback to standard dot-notation pattern:
    // obj.reverse(a); obj.swap(a, 12); obj.splice(a, 3);
    let dot_call_re = regex::Regex::new(&format!(
        r#"{}\.([\w$]+)\([a-zA-Z0-9_$]+(?:\s*,\s*(\d+))?\)"#,
        regex::escape(obj_name)
    )).ok()?;

    let mut ops = Vec::new();
    for m in dot_call_re.captures_iter(js) {
        let fn_name = m.get(1)?.as_str();
        let arg: usize = m.get(2).and_then(|s| s.as_str().parse().ok()).unwrap_or(0);

        if fn_name == rev_fn || fn_name == "reverse" {
            ops.push(DecipherOp::Reverse);
        } else if fn_name == swap_fn || fn_name == "swap" {
            ops.push(DecipherOp::Swap(arg));
        } else if fn_name == splice_fn || fn_name == "splice" {
            ops.push(DecipherOp::Splice(arg));
        }
    }

    if !ops.is_empty() {
        Some(ops)
    } else {
        None
    }
}

/// Strip IDM chunk/state query parameters (String 48: #sq, #rbuf, #range, #ump, #begin, #len)
pub fn sanitize_stream_url(raw_url: &str) -> Result<String> {
    let mut url = Url::parse(raw_url).context("Failed to parse stream URL")?;
    let strip_params = [
        "begin", "len", "range", "rbuf", "sq", "ump", "beids", "headm",
    ];

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !strip_params.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url.into())
}

/// Extract YouTube Video ID from any valid YouTube URL
pub fn extract_video_id(url: &str) -> Option<String> {
    if let Ok(parsed) = Url::parse(url) {
        if let Some(domain) = parsed.domain() {
            let dom = domain.to_lowercase();
            if dom.contains("youtu.be") {
                return parsed
                    .path_segments()?
                    .next()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
            }
            if dom.contains("youtube.com") {
                for (k, v) in parsed.query_pairs() {
                    if k == "v" && !v.is_empty() {
                        return Some(v.into_owned());
                    }
                }
                if let Some(mut segs) = parsed.path_segments() {
                    if let Some(first) = segs.next() {
                        if first == "shorts" || first == "embed" || first == "v" {
                            if let Some(next) = segs.next() {
                                if !next.is_empty() {
                                    return Some(next.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Pure Rust YouTube Media Resolver (100% Native | Zero external tools)
pub async fn resolve_youtube_media(url: &str) -> Result<ResolvedMedia> {
    let video_id = extract_video_id(url).ok_or_else(|| {
        anyhow!("Invalid YouTube URL: could not extract 11-character video ID from '{}'", url)
    })?;

    info!("InnerTube: Resolving YouTube media for videoId='{}'", video_id);

    let client = reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(std::time::Duration::from_secs(12))
        .build()?;

    // 1. Fetch watch page HTML
    let watch_url = format!("https://www.youtube.com/watch?v={}", video_id);
    let mut req_headers = HeaderMap::new();
    req_headers.insert(REFERER, HeaderValue::from_static("https://www.youtube.com/"));
    req_headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );

    let resp = client
        .get(&watch_url)
        .headers(req_headers.clone())
        .send()
        .await
        .context("Failed to fetch YouTube watch page")?;

    let html = resp.text().await?;

    // 2. Extract ytInitialPlayerResponse JSON
    let player_data = extract_player_response(&html).context("Failed to parse ytInitialPlayerResponse from watch page")?;

    let title = player_data
        .pointer("/videoDetails/title")
        .and_then(|v| v.as_str())
        .unwrap_or("youtube_video")
        .to_string();

    let streaming_data = player_data
        .get("streamingData")
        .ok_or_else(|| anyhow!("YouTube video has no streamingData (may be private or age-restricted)"))?;

    // 3. Inspect progressive formats and adaptive formats
    let formats = streaming_data.get("formats").and_then(|v| v.as_array());
    let adaptive_formats = streaming_data.get("adaptiveFormats").and_then(|v| v.as_array());

    let mut selected_video_url: Option<String> = None;
    let mut selected_audio_url: Option<String> = None;
    let mut total_size: Option<u64> = None;

    // Check progressive first (itag 18 or 22)
    if let Some(fmts) = formats {
        for fmt in fmts {
            if let Some(direct_url) = fmt.get("url").and_then(|u| u.as_str()) {
                selected_video_url = Some(direct_url.to_string());
                if let Some(clen) = fmt.get("contentLength").and_then(|c| c.as_str()).and_then(|s| s.parse::<u64>().ok()) {
                    total_size = Some(clen);
                }
                break;
            }
        }
    }

    // Check adaptive formats (1080p itag 137, 720p itag 136, audio itag 140)
    if selected_video_url.is_none() {
        if let Some(ad_fmts) = adaptive_formats {
            let mut best_video: Option<&Value> = None;
            let mut best_audio: Option<&Value> = None;

            for fmt in ad_fmts {
                let mime = fmt.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");
                if mime.starts_with("video/") {
                    let height = fmt.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                    let current_best_h = best_video
                        .and_then(|v| v.get("height"))
                        .and_then(|h| h.as_u64())
                        .unwrap_or(0);
                    if height > current_best_h || best_video.is_none() {
                        best_video = Some(fmt);
                    }
                } else if mime.starts_with("audio/") {
                    let bitrate = fmt.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0);
                    let current_best_b = best_audio
                        .and_then(|a| a.get("bitrate"))
                        .and_then(|b| b.as_u64())
                        .unwrap_or(0);
                    if bitrate > current_best_b || best_audio.is_none() {
                        best_audio = Some(fmt);
                    }
                }
            }

            // Extract player base.js for signature deciphering if needed
            let mut decipherer: Option<Decipherer> = None;
            if let Some(v_fmt) = best_video {
                if v_fmt.get("signatureCipher").is_some() || v_fmt.get("cipher").is_some() {
                    decipherer = fetch_and_build_decipherer(&client, &html).await;
                }
            }

            if let Some(v_fmt) = best_video {
                selected_video_url = resolve_format_url(v_fmt, decipherer.as_ref());
                let v_size = v_fmt.get("contentLength").and_then(|c| c.as_str()).and_then(|s| s.parse::<u64>().ok());
                let a_size = best_audio.and_then(|a| a.get("contentLength")).and_then(|c| c.as_str()).and_then(|s| s.parse::<u64>().ok());

                if let (Some(vs), Some(as_)) = (v_size, a_size) {
                    total_size = Some(vs + as_);
                } else {
                    total_size = v_size.or(a_size);
                }
            }

            if let Some(a_fmt) = best_audio {
                selected_audio_url = resolve_format_url(a_fmt, decipherer.as_ref());
            }
        }
    }

    let final_video_url = selected_video_url
        .ok_or_else(|| anyhow!("Could not resolve direct or ciphered video stream URL for YouTube video '{}'", video_id))?;

    let clean_video_url = sanitize_stream_url(&final_video_url)?;
    let clean_audio_url = selected_audio_url.and_then(|u| sanitize_stream_url(&u).ok());

    let clean_title = crate::prober::sanitize_filename(&title);
    let filename = format!("{}.mp4", clean_title.trim_matches('_'));

    let mut headers = HashMap::new();
    headers.insert("Referer".to_string(), "https://www.youtube.com/".to_string());
    headers.insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());

    info!(
        "InnerTube: Successfully resolved media: title='{}', filename='{}', has_audio={}",
        title, filename, clean_audio_url.is_some()
    );

    Ok(ResolvedMedia {
        stream_url: clean_video_url,
        audio_url: clean_audio_url,
        filename,
        title,
        total_size,
        headers,
        user_agent: Some(DEFAULT_USER_AGENT.to_string()),
    })
}

fn resolve_format_url(fmt: &Value, decipherer: Option<&Decipherer>) -> Option<String> {
    if let Some(url) = fmt.get("url").and_then(|u| u.as_str()) {
        return Some(url.to_string());
    }

    let cipher = fmt.get("signatureCipher").or_else(|| fmt.get("cipher"))?.as_str()?;
    let parsed_url = Url::parse(&format!("https://dummy.com/?{}", cipher)).ok()?;

    let mut raw_stream_url = String::new();
    let mut s_sig = String::new();
    let mut sp = "sig".to_string();

    for (k, v) in parsed_url.query_pairs() {
        if k == "url" {
            raw_stream_url = v.into_owned();
        } else if k == "s" {
            s_sig = v.into_owned();
        } else if k == "sp" {
            sp = v.into_owned();
        }
    }

    if raw_stream_url.is_empty() {
        return None;
    }

    let deciphered_sig = if let Some(d) = decipherer {
        d.decipher(&s_sig)
    } else {
        s_sig
    };

    let separator = if raw_stream_url.contains('?') { "&" } else { "?" };
    Some(format!("{}{}{}={}", raw_stream_url, separator, sp, deciphered_sig))
}

async fn fetch_and_build_decipherer(client: &reqwest::Client, html: &str) -> Option<Decipherer> {
    let base_js_re = regex::Regex::new(r#"(/s/player/[^"]+?/base\.js)"#).ok()?;
    let path = base_js_re.captures(html)?.get(1)?.as_str();
    let base_js_url = format!("https://www.youtube.com{}", path);

    info!("InnerTube: Fetching player base.js from {}", base_js_url);
    let js_text = client.get(&base_js_url).send().await.ok()?.text().await.ok()?;

    Decipherer::parse_from_js(&js_text)
}

fn extract_player_response(html: &str) -> Option<Value> {
    let re1 = regex::Regex::new(r"ytInitialPlayerResponse\s*=\s*(\{.+?\});").ok()?;
    if let Some(caps) = re1.captures(html) {
        if let Ok(v) = serde_json::from_str::<Value>(caps.get(1)?.as_str()) {
            return Some(v);
        }
    }

    let re2 = regex::Regex::new(r"var ytInitialPlayerResponse\s*=\s*(\{.+?\});").ok()?;
    if let Some(caps) = re2.captures(html) {
        if let Ok(v) = serde_json::from_str::<Value>(caps.get(1)?.as_str()) {
            return Some(v);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_video_id() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=5gg17XXXiNo").as_deref(),
            Some("5gg17XXXiNo")
        );
        assert_eq!(
            extract_video_id("https://youtu.be/5gg17XXXiNo").as_deref(),
            Some("5gg17XXXiNo")
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/shorts/5gg17XXXiNo").as_deref(),
            Some("5gg17XXXiNo")
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/embed/5gg17XXXiNo").as_deref(),
            Some("5gg17XXXiNo")
        );
        assert_eq!(
            extract_video_id("https://example.com/video"),
            None
        );
    }

    #[test]
    fn test_sanitize_stream_url() {
        let raw = "https://rr1.googlevideo.com/videoplayback?expire=123&sq=45&range=0-1000&rbuf=20&ump=1&sig=ABC&sparams=expire&n=XYZ";
        let clean = sanitize_stream_url(raw).unwrap();
        assert!(!clean.contains("sq="));
        assert!(!clean.contains("range="));
        assert!(!clean.contains("rbuf="));
        assert!(!clean.contains("ump="));
        assert!(clean.contains("sig=ABC"));
        assert!(clean.contains("sparams=expire"));
        assert!(clean.contains("n=XYZ"));
    }

    #[test]
    fn test_decipherer_primitives() {
        let decipherer = Decipherer {
            operations: vec![
                DecipherOp::Reverse,
                DecipherOp::Splice(2),
                DecipherOp::Swap(1),
            ],
        };
        // Input: "ABCDEF"
        // 1. Reverse: "FEDCBA"
        // 2. Splice 2: "DCBA"
        // 3. Swap 1: swap index 0 and 1 -> "CDBA"
        let res = decipherer.decipher("ABCDEF");
        assert_eq!(res, "CDBA");
    }
}
