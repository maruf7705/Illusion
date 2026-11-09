//! Immutable media-capture data passed from the browser integration to the
//! downloader.  A captured URL is a capability: signed CDN query parameters
//! must be replayed exactly as the browser observed them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCandidateKind {
    Progressive,
    PairedAudioVideo,
    HlsManifest,
    DashManifest,
    ProtectedOrUnsupported,
}

impl Default for MediaCandidateKind {
    fn default() -> Self {
        Self::Progressive
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestContext {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content_length: Option<u64>,
    #[serde(default)]
    pub accept_ranges: Option<bool>,
}

impl RequestContext {
    pub fn normalized_headers(&self) -> HashMap<String, String> {
        let mut headers = self.headers.clone();
        if let Some(referer) = self.referer.as_ref().filter(|value| !value.is_empty()) {
            headers.entry("referer".to_string()).or_insert_with(|| referer.clone());
        }
        if let Some(origin) = self.origin.as_ref().filter(|value| !value.is_empty()) {
            headers.entry("origin".to_string()).or_insert_with(|| origin.clone());
        }
        headers
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaCandidate {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: MediaCandidateKind,
    #[serde(default)]
    pub page_url: Option<String>,
    pub primary: RequestContext,
    #[serde(default)]
    pub audio: Option<RequestContext>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub codecs: Option<String>,
    #[serde(default)]
    pub unsupported_reason: Option<String>,
}

impl MediaCandidate {
    pub fn progressive(url: String, headers: HashMap<String, String>) -> Self {
        Self {
            id: None,
            kind: MediaCandidateKind::Progressive,
            page_url: None,
            primary: RequestContext {
                url,
                headers,
                ..RequestContext::default()
            },
            audio: None,
            quality: None,
            codecs: None,
            unsupported_reason: None,
        }
    }

    pub fn expected_size(&self) -> Option<u64> {
        match (&self.primary.content_length, &self.audio) {
            (Some(primary), Some(audio)) => Some(primary.saturating_add(audio.content_length.unwrap_or(0))),
            (Some(primary), None) => Some(*primary),
            (None, Some(audio)) => audio.content_length,
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context_keeps_the_signed_url_and_adds_context_headers() {
        let context = RequestContext {
            url: "https://example.invalid/video?sig=unchanged&range=1-2".to_string(),
            referer: Some("https://example.invalid/watch".to_string()),
            origin: Some("https://example.invalid".to_string()),
            ..RequestContext::default()
        };

        assert!(context.url.contains("sig=unchanged&range=1-2"));
        let headers = context.normalized_headers();
        assert_eq!(headers.get("referer").unwrap(), "https://example.invalid/watch");
        assert_eq!(headers.get("origin").unwrap(), "https://example.invalid");
    }
}
