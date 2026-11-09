use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE};
use std::collections::HashMap;
use url::Url;

/// IDM-Grade Media URL Sanitizer:
/// Strips subsegment chunk bounds (`bytestart`, `byteend`, `range`, `sq`, `segment`)
/// so that multi-segment parallel HTTP Range requests can download the entire file.
/// CRITICAL: Preserves all HMAC parameters signed in `sparams` (e.g. YouTube GoogleVideo).
pub fn clean_media_url(raw_url: &str) -> String {
    crate::stream_engine::clean_stream_url(raw_url)
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
    let clean_url = clean_media_url(url_str);
    let parsed_url = Url::parse(&clean_url)?;
    let mut custom_headers = HeaderMap::new();

    if let Some(headers) = extra_headers {
        for (k, v) in headers {
            let k_lower = k.to_lowercase();
            if k_lower == "host" || k_lower == "content-length" || k_lower == "origin" 
               || k_lower.starts_with("sec-fetch-") {
                continue;
            }
            if let (Ok(name), Ok(val)) = (k.parse::<reqwest::header::HeaderName>(), HeaderValue::from_str(v)) {
                custom_headers.insert(name, val);
            }
        }
    }

    if !custom_headers.contains_key(reqwest::header::USER_AGENT) {
        custom_headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"),
        );
    }

    let is_googlevideo = parsed_url.host_str().map_or(false, |h| h.contains("googlevideo.com"));
    if is_googlevideo {
        if !custom_headers.contains_key(reqwest::header::REFERER) {
            custom_headers.insert(
                reqwest::header::REFERER,
                HeaderValue::from_static("https://www.youtube.com/"),
            );
        }
    }

    // Try HEAD request first (skip for googlevideo/streaming CDNs which reject HEAD with 403)
    let resp = if is_googlevideo {
        None
    } else {
        let head_req = client.head(&clean_url).headers(custom_headers.clone());
        head_req.send().await.ok()
    };

    let mut total_size: Option<u64> = None;
    let mut accept_ranges = false;
    let mut etag = None;
    let mut last_modified = None;
    let mut content_type = None;
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

    // Check for clen in query parameters (common in streaming media like YouTube/GoogleVideo)
    for (k, v) in parsed_url.query_pairs() {
        if k == "clen" {
            if let Ok(s) = v.parse::<u64>() {
                total_size = Some(s);
                accept_ranges = true;
            }
            break;
        }
    }

    // If HEAD failed or gave no content-length, and total_size is still unknown, probe with Range GET bytes=0-0
    // (If total_size is already known via clen or HEAD, skip GET bytes=0-0 to avoid 403 Forbidden from CDNs)
    if total_size.is_none() {
        let mut probe_headers = custom_headers.clone();
        probe_headers.insert(RANGE, HeaderValue::from_static("bytes=0-0"));

        let get_resp = client.get(&clean_url).headers(probe_headers).send().await?;
        let status = get_resp.status();
        let headers = get_resp.headers();

        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() == 403 && is_googlevideo {
                // Google Video CDN returns 403 Forbidden on bytes=0-0 if tokens are strict;
                // Treat as streamable with ranges enabled
                accept_ranges = true;
            } else {
                return Err(anyhow::anyhow!("Remote server returned HTTP status {}", status));
            }
        } else if status.as_u16() == 206 {
            accept_ranges = true;
            // Parse Content-Range: bytes 0-0/123456
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
        } else if status.is_success() {
            // Server ignored Range header, sends entire body
            accept_ranges = false;
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
        url: clean_url,
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
