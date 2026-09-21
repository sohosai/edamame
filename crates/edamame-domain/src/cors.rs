//! ブラウザからの読み取りを許可する origin を設定する

use url::Url;

use crate::error::EdamameError;

/// ブラウザからの読み取りを許可する origin の集合を表す
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsOrigins {
    /// CORS を有効にしない
    Disabled,
    /// すべての origin を許可する
    Any,
    /// リストに含まれる origin だけを許可する
    List(Vec<String>),
}

impl CorsOrigins {
    /// CLI で受け取った値を検証し、許可する origin の設定に変換する
    pub fn parse(values: &[String]) -> Result<Self, EdamameError> {
        if values.is_empty() {
            return Ok(Self::Disabled);
        }
        // `none` と `*` は許可する origin の並びと意味が重なるため、単独のときだけ受け付ける
        if values.iter().any(|value| value == "none") {
            if values.len() > 1 {
                return Err(EdamameError::BadRequest(
                    "cors origin `none` cannot be combined with other values".to_owned(),
                ));
            }
            return Ok(Self::Disabled);
        }
        if values.iter().any(|value| value == "*") {
            if values.len() > 1 {
                return Err(EdamameError::BadRequest(
                    "cors origin `*` cannot be combined with other values".to_owned(),
                ));
            }
            return Ok(Self::Any);
        }
        let mut origins = Vec::new();
        for value in values {
            let origin = normalize_origin(value)?;
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        Ok(Self::List(origins))
    }
}

/// origin を検証し、末尾の `/` を取り除いて返す
fn normalize_origin(value: &str) -> Result<String, EdamameError> {
    // file:// で開いたページの Origin は `null` になる
    if value == "null" {
        return Ok(value.to_owned());
    }
    let url = Url::parse(value)
        .map_err(|_| EdamameError::BadRequest(format!("cors origin is invalid: {value}")))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EdamameError::BadRequest(format!(
            "cors origin scheme must be http or https: {value}"
        )));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(EdamameError::BadRequest(format!(
            "cors origin must contain a host and no user information: {value}"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(EdamameError::BadRequest(format!(
            "cors origin must not contain a query or fragment: {value}"
        )));
    }
    if !url.path().is_empty() && url.path() != "/" {
        return Err(EdamameError::BadRequest(format!(
            "cors origin must not contain a path: {value}"
        )));
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

    fn parse(values: &[&str]) -> Result<CorsOrigins, EdamameError> {
        let values = values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        CorsOrigins::parse(&values)
    }

    #[test]
    fn treats_missing_values_as_disabled() {
        assert_eq!(parse(&[]).unwrap(), CorsOrigins::Disabled);
        assert_eq!(parse(&["none"]).unwrap(), CorsOrigins::Disabled);
    }

    #[test]
    fn accepts_wildcard() {
        assert_eq!(parse(&["*"]).unwrap(), CorsOrigins::Any);
    }

    #[test]
    fn normalizes_origins() {
        assert_eq!(
            parse(&["null"]).unwrap(),
            CorsOrigins::List(vec!["null".to_owned()])
        );
        assert_eq!(
            parse(&["http://127.0.0.1:9000/"]).unwrap(),
            CorsOrigins::List(vec!["http://127.0.0.1:9000".to_owned()])
        );
        assert_eq!(
            parse(&["https://page.test", "https://page.test"]).unwrap(),
            CorsOrigins::List(vec!["https://page.test".to_owned()])
        );
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(parse(&[""]).is_err());
        assert!(parse(&["none", "*"]).is_err());
        assert!(parse(&["none", "http://page.test"]).is_err());
        assert!(parse(&["*", "http://page.test"]).is_err());
        assert!(parse(&["page.test"]).is_err());
        assert!(parse(&["ftp://page.test"]).is_err());
        assert!(parse(&["http://user@page.test"]).is_err());
        assert!(parse(&["http://page.test/path"]).is_err());
        assert!(parse(&["http://page.test?x=1"]).is_err());
        assert!(parse(&["http://page.test#f"]).is_err());
    }
}
