//! `public_url`、`controller_url`、暗号鍵の URI の検証と正規化

use url::Url;

use crate::error::EdamameError;

/// `http://` の絶対 URL を検証し、末尾の `/` を取り除いて返す
pub fn normalize_http_url(value: &str) -> Result<String, EdamameError> {
    let url = Url::parse(value)
        .map_err(|_| EdamameError::BadRequest("url must be an absolute http URL".to_owned()))?;
    if url.scheme() != "http" {
        return Err(EdamameError::BadRequest(
            "url scheme must be http".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(EdamameError::BadRequest(
            "url must contain a host".to_owned(),
        ));
    }
    if url.query().is_some() {
        return Err(EdamameError::BadRequest(
            "url must not contain a query".to_owned(),
        ));
    }
    if url.fragment().is_some() {
        return Err(EdamameError::BadRequest(
            "url must not contain a fragment".to_owned(),
        ));
    }
    let mut normalized = url.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

/// 暗号鍵の URI を検証し、正規形を返す
///
/// 鍵は publisher が配信するため、`http` と `https` の絶対 URL だけを許可する。
pub fn normalize_key_uri(value: &str) -> Result<String, EdamameError> {
    let url = Url::parse(value)
        .map_err(|_| EdamameError::BadRequest("key_uri must be an absolute URL".to_owned()))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EdamameError::BadRequest(
            "key_uri scheme must be http or https".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(EdamameError::BadRequest(
            "key_uri must contain a host".to_owned(),
        ));
    }
    if url.fragment().is_some() {
        return Err(EdamameError::BadRequest(
            "key_uri must not contain a fragment".to_owned(),
        ));
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_http_url() {
        assert_eq!(
            normalize_http_url("http://127.0.0.1:9001/").unwrap(),
            "http://127.0.0.1:9001"
        );
        assert_eq!(
            normalize_http_url("http://example.test/base/").unwrap(),
            "http://example.test/base"
        );
    }

    #[test]
    fn rejects_invalid_urls() {
        assert!(normalize_http_url("https://example.test").is_err());
        assert!(normalize_http_url("/relative").is_err());
        assert!(normalize_http_url("http://example.test/?x=1").is_err());
        assert!(normalize_http_url("http://example.test/#f").is_err());
    }

    #[test]
    fn normalizes_key_uri() {
        assert_eq!(
            normalize_key_uri("https://keys.example.test/live/key-1").unwrap(),
            "https://keys.example.test/live/key-1"
        );
        // 鍵の配信元は query で認証することがあるため、query は許可する
        assert_eq!(
            normalize_key_uri("http://127.0.0.1:8000/key?token=1").unwrap(),
            "http://127.0.0.1:8000/key?token=1"
        );
        assert_eq!(
            normalize_key_uri("https://keys.example.test").unwrap(),
            "https://keys.example.test/"
        );
    }

    #[test]
    fn rejects_invalid_key_uri() {
        assert!(normalize_key_uri("/keys/key-1").is_err());
        assert!(normalize_key_uri("").is_err());
        assert!(normalize_key_uri("ftp://keys.example.test/key").is_err());
        assert!(normalize_key_uri("file:///tmp/key").is_err());
        assert!(normalize_key_uri("https://keys.example.test/key#fragment").is_err());
    }
}
