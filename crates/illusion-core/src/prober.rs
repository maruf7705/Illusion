use crate::media::RequestContext;
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE};
use std::collections::HashMap;
use url::Url;

/// A browser-captured media URL is immutable. In particular, signed CDN URLs
/// often include parameters that look like ranges but are part of the request
/// capability. The downloader adds its own `Range` header when appropriate.
pub fn clean_media_url(raw_url: &str) -> String {
    raw_url.to_string()
}

pub fn build_request_headers(
    extra_headers: &HashMap<String, String>,
    cookies: Option<&str>,
    user_agent: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in extra_headers {
        let lower = key.to_ascii_lowercase();
        // These are generated for each download request and must never be
        // copied from a browser connection. `Origin`, `Referer`, auth headers,
        // and cookies are intentionally retained when captured.
        if matches!(lower.as_str(), "host" | "content-length" | "range" | "accept-encoding" | "connection")
            || lower.starts_with("sec-fetch-")
            || lower.starts_with("sec-ch-")
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (key.parse::<reqwest::header::HeaderName>(), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    }

    if let Some(value) = user_agent.filter(|value| !value.is_empty()) {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(reqwest::header::USER_AGENT, value);
        }
    }
    if !headers.contains_key(reqwest::header::USER_AGENT) {
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"),
        );
    }
    if let Some(value) = cookies.filter(|value| !value.is_empty()) {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(reqwest::header::COOKIE, value);
        }
    }
    headers
}

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub url: String,
    pub filename: String,
    pub total_size: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

pub async fn probe_url(
    client: &reqwest::Client,
    url_str: &str,
    extra_headers: Option<&HashMap<String, String>>,
    suggested_filename: Option<&str>,
) -> Result<RemoteInfo> {
    let context = RequestContext {
        url: url_str.to_string(),
        headers: extra_headers.cloned().unwrap_or_default(),
        ..RequestContext::default()
    };
    probe_request(client, &context, suggested_filename).await
}

pub async fn probe_request(
    client: &reqwest::Client,
    context: &RequestContext,
    suggested_filename: Option<&str>,
) -> Result<RemoteInfo> {
    let raw_url = clean_media_url(&context.url);
    let parsed_url = Url::parse(&raw_url)?;
    let custom_headers = build_request_headers(
        &context.normalized_headers(),
        context.cookies.as_deref(),
        context.user_agent.as_deref(),
    );
    let content_type_hint = context.content_type.as_deref().unwrap_or_default().to_ascii_lowercase();
    let is_manifest = raw_url.contains(".m3u8")
        || raw_url.contains(".mpd")
        || content_type_hint.contains("mpegurl")
        || content_type_hint.contains("dash+xml");

    // Try HEAD request first (skip for googlevideo/streaming CDNs and segmented playlists)
    let resp = if is_manifest {
        None
    } else {
        let head_req = client.head(&raw_url).headers(custom_headers.clone());
        head_req.send().await.ok()
    };

    let mut total_size: Option<u64> = context.content_length;
    let mut accept_ranges = false;
    let mut etag = None;
    let mut last_modified = None;
    let mut content_type = if raw_url.contains(".mpd") {
        Some("application/dash+xml".to_string())
    } else if raw_url.contains(".m3u8") {
        Some("application/vnd.apple.mpegurl".to_string())
    } else if !content_type_hint.is_empty() {
        Some(content_type_hint)
    } else {
        None
    };
    let mut disposition_filename = None;

    if let Some(ref r) = resp {
        if r.status().is_success() {
            let headers = r.headers();

            if let Some(cl) = headers.get(CONTENT_LENGTH) {
                if let Ok(val) = cl.to_str() {
                    total_size = val.trim().parse::<u64>().ok();
                }
            }

            if let Some(ar) = headers.get(ACCEPT_RANGES) {
                if let Ok(val) = ar.to_str() {
                    accept_ranges = val.trim().eq_ignore_ascii_case("bytes");
                }
            }

            if let Some(et) = headers.get(ETAG) {
                if let Ok(val) = et.to_str() {
                    etag = Some(val.to_string());
                }
            }

            if let Some(lm) = headers.get(LAST_MODIFIED) {
                if let Ok(val) = lm.to_str() {
                    last_modified = Some(val.to_string());
                }
            }

            if let Some(ct) = headers.get(CONTENT_TYPE) {
                if let Ok(val) = ct.to_str() {
                    content_type = Some(val.to_string());
                }
            }

            if let Some(cd) = headers.get(CONTENT_DISPOSITION) {
                if let Ok(val) = cd.to_str() {
                    disposition_filename = parse_content_disposition_filename(val);
                }
            }
        }
    }

    // Check for clen / filesize in query parameters (common in streaming media like YouTube/GoogleVideo)
    for (k, v) in parsed_url.query_pairs() {
        if k.eq_ignore_ascii_case("clen") || k.eq_ignore_ascii_case("filesize") || k.eq_ignore_ascii_case("size") {
            if let Ok(s) = v.parse::<u64>() {
                if s > 0 {
                    total_size = Some(s);
                    accept_ranges = true;
                    break;
                }
            }
        }
    }

    // If HEAD failed or gave no content-length, and total_size is still unknown, probe with Range GET bytes=0-
    // (skip for HLS/DASH manifest playlists)
    if total_size.is_none() && !is_manifest {
        let mut probe_headers = custom_headers.clone();
        probe_headers.insert(RANGE, HeaderValue::from_static("bytes=0-"));

        let get_resp = client.get(&raw_url).headers(probe_headers).send().await;
        if let Ok(resp) = get_resp {
            let status = resp.status();
            let headers = resp.headers();

            if status.is_success() || status.as_u16() == 206 {
                if status.as_u16() == 206 {
                    accept_ranges = true;
                }
                // Parse Content-Range: bytes 0-XXXXX/123456
                if let Some(cr) = headers.get("content-range") {
                    if let Ok(val) = cr.to_str() {
                        if let Some(pos) = val.rfind('/') {
                            let total_part = &val[pos + 1..].trim();
                            if let Ok(s) = total_part.parse::<u64>() {
                                total_size = Some(s);
                            }
                        }
                    }
                }
                if total_size.is_none() {
                    if let Some(cl) = headers.get(CONTENT_LENGTH) {
                        if let Ok(val) = cl.to_str() {
                            total_size = val.trim().parse::<u64>().ok();
                        }
                    }
                }
                if disposition_filename.is_none() {
                    if let Some(cd) = headers.get(CONTENT_DISPOSITION) {
                        if let Ok(val) = cd.to_str() {
                            disposition_filename = parse_content_disposition_filename(val);
                        }
                    }
                }
                if content_type.is_none() {
                    if let Some(ct) = headers.get(CONTENT_TYPE) {
                        if let Ok(val) = ct.to_str() {
                            content_type = Some(val.to_string());
                        }
                    }
                }
            }
        }
    }

    // Resolve final filename
    let final_filename = if let Some(suggested) = suggested_filename {
        if !suggested.trim().is_empty() {
            sanitize_filename(suggested)
        } else {
            resolve_filename_from_url_or_cd(&parsed_url, disposition_filename)
        }
    } else {
        resolve_filename_from_url_or_cd(&parsed_url, disposition_filename)
    };

    Ok(RemoteInfo {
        url: raw_url,
        filename: final_filename,
        total_size,
        accept_ranges,
        etag,
        last_modified,
        content_type,
    })
}

fn resolve_filename_from_url_or_cd(url: &Url, cd_filename: Option<String>) -> String {
    if let Some(cd) = cd_filename {
        return sanitize_filename(&cd);
    }

    let path_segments: Vec<_> = url.path_segments().map(|c| c.collect()).unwrap_or_default();
    if let Some(last) = path_segments.last() {
        if !last.trim().is_empty() {
            return sanitize_filename(last);
        }
    }

    "download".to_string()
}

pub fn parse_content_disposition_filename(header_val: &str) -> Option<String> {
    for part in header_val.split(';') {
        let trimmed = part.trim();
        if let Some(idx) = trimmed.find("filename*=") {
            // RFC 5987: filename*=UTF-8''filename.ext
            let raw = &trimmed[idx + 10..].trim();
            if let Some(quote_idx) = raw.rfind("''") {
                let encoded = &raw[quote_idx + 2..].trim_matches('"');
                if let Ok(decoded) = urlencoding::decode(encoded) {
                    return Some(decoded.into_owned());
                }
            }
        } else if let Some(idx) = trimmed.find("filename=") {
            let raw = trimmed[idx + 9..].trim().trim_matches('"');
            return Some(raw.to_string());
        }
    }
    None
}

pub fn sanitize_filename(name: &str) -> String {
    let clean = name.replace(['/', '\\', '?', '%', '*', ':', '|', '"', '<', '>', '\0'], "_");
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("valid_name.mp4"), "valid_name.mp4");
        assert_eq!(sanitize_filename("invalid/name:test*?.mp4"), "invalid_name_test__.mp4");
        assert_eq!(sanitize_filename("   "), "download");
    }

    #[test]
    fn test_content_disposition_filename() {
        let cd = "attachment; filename=\"video_sample.mp4\"";
        assert_eq!(parse_content_disposition_filename(cd), Some("video_sample.mp4".to_string()));

        let cd_utf8 = "attachment; filename*=UTF-8''my%20song.mp3";
        assert_eq!(parse_content_disposition_filename(cd_utf8), Some("my song.mp3".to_string()));
    }
}
