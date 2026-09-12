//! `public_url` と `controller_url` の検証と正規化

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
}
