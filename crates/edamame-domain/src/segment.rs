//! セグメントの識別子、ファイル名、解像度を検証する

use serde::{Deserialize, Serialize};

use crate::error::EdamameError;

/// 識別子の最大長をバイト単位で表す
pub const MAX_IDENTIFIER_LEN: usize = 64;

/// channel、quality、セグメント番号の組でセグメントを識別する。
/// `new` で作成するときに channel と quality の識別子を検証する。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentId {
    pub channel_id: String,
    pub quality: String,
    pub sequence: u64,
}

impl SegmentId {
    /// channel と quality の識別子を検証して、セグメントを識別する組を作る
    pub fn new(
        channel_id: impl Into<String>,
        quality: impl Into<String>,
        sequence: u64,
    ) -> Result<Self, EdamameError> {
        let channel_id = channel_id.into();
        let quality = quality.into();
        validate_identifier("channel_id", &channel_id)?;
        validate_quality(&quality)?;
        Ok(Self {
            channel_id,
            quality,
            sequence,
        })
    }
}

/// sync の JSON でセグメントを識別するために使う
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SegmentKey {
    pub channel_id: String,
    pub quality: String,
    pub sequence: u64,
}

impl From<&SegmentId> for SegmentKey {
    fn from(id: &SegmentId) -> Self {
        Self {
            channel_id: id.channel_id.clone(),
            quality: id.quality.clone(),
            sequence: id.sequence,
        }
    }
}

impl SegmentKey {
    /// channel と quality の識別子を検証して、`SegmentId` に変換する
    pub fn into_id(self) -> Result<SegmentId, EdamameError> {
        SegmentId::new(self.channel_id, self.quality, self.sequence)
    }
}

/// 識別子の文字種と長さを検証する
pub fn validate_identifier(kind: &str, value: &str) -> Result<(), EdamameError> {
    if value.is_empty() {
        return Err(EdamameError::BadRequest(format!(
            "{kind} must not be empty"
        )));
    }
    if value.len() > MAX_IDENTIFIER_LEN {
        return Err(EdamameError::BadRequest(format!(
            "{kind} must be at most {MAX_IDENTIFIER_LEN} bytes"
        )));
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(EdamameError::BadRequest(format!(
            "{kind} contains an invalid character: {value}"
        )));
    }
    Ok(())
}

/// quality の識別子を検証する。
/// `master` は予約語として拒否する。
pub fn validate_quality(value: &str) -> Result<(), EdamameError> {
    validate_identifier("quality", value)?;
    if value == "master" {
        return Err(EdamameError::BadRequest(
            "quality master is reserved".to_owned(),
        ));
    }
    Ok(())
}

/// `{sequence}.ts` のファイル名をセグメント番号に変換する
pub fn parse_sequence_filename(filename: &str) -> Result<u64, EdamameError> {
    let stem = filename
        .strip_suffix(".ts")
        .ok_or_else(|| EdamameError::BadRequest("filename must end with .ts".to_owned()))?;
    if stem.is_empty() || !stem.chars().all(|character| character.is_ascii_digit()) {
        return Err(EdamameError::BadRequest(
            "filename must be a decimal sequence with .ts".to_owned(),
        ));
    }
    if stem.len() > 1 && stem.starts_with('0') {
        return Err(EdamameError::BadRequest(
            "filename sequence must not have leading zeros".to_owned(),
        ));
    }
    stem.parse::<u64>()
        .map_err(|_| EdamameError::BadRequest("filename sequence is too large".to_owned()))
}

/// セグメント番号からファイル名を作る
#[must_use]
pub fn render_filename(sequence: u64) -> String {
    format!("{sequence}.ts")
}

/// `{width}x{height}` 形式の解像度を検証する
pub fn validate_resolution(value: &str) -> Result<(), EdamameError> {
    let mut parts = value.split('x');
    let width = parts.next().unwrap_or_default();
    let height = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || width.is_empty()
        || height.is_empty()
        || !width.chars().all(|character| character.is_ascii_digit())
        || !height.chars().all(|character| character.is_ascii_digit())
    {
        return Err(EdamameError::BadRequest(
            "resolution must be WIDTHxHEIGHT".to_owned(),
        ));
    }
    Ok(())
}

/// IV を表す 16 byte 分の hex を小文字にそろえる
pub fn normalize_iv(value: &str) -> Result<String, EdamameError> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(EdamameError::BadRequest(
            "iv must be 32 hex characters".to_owned(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_identifiers() {
        assert!(validate_identifier("channel_id", "live").is_ok());
        assert!(validate_identifier("channel_id", "live_1").is_ok());
        assert!(validate_identifier("channel_id", "").is_err());
        assert!(validate_identifier("channel_id", "../live").is_err());
        assert!(validate_identifier("channel_id", "live room").is_err());
        assert!(validate_quality("master").is_err());
    }

    #[test]
    fn parses_sequence_filename() {
        assert_eq!(parse_sequence_filename("101.ts").unwrap(), 101);
        assert!(parse_sequence_filename("101.m4s").is_err());
        assert!(parse_sequence_filename("001.ts").is_err());
        assert!(parse_sequence_filename("abc.ts").is_err());
        assert!(parse_sequence_filename("101").is_err());
        assert_eq!(render_filename(101), "101.ts");
    }

    #[test]
    fn validates_resolution() {
        assert!(validate_resolution("1280x720").is_ok());
        assert!(validate_resolution("1280X720").is_err());
        assert!(validate_resolution("1280").is_err());
        assert!(validate_resolution("x720").is_err());
        assert!(validate_resolution("1280x720x1").is_err());
    }

    #[test]
    fn normalizes_iv() {
        assert_eq!(
            normalize_iv("00000000000000000000000000000000").unwrap(),
            "00000000000000000000000000000000"
        );
        // 大文字は小文字にそろえる
        assert_eq!(
            normalize_iv("0000000000000000000000000000000A").unwrap(),
            "0000000000000000000000000000000a"
        );
        assert!(normalize_iv("0000").is_err());
        assert!(normalize_iv("0000000000000000000000000000000z").is_err());
        assert!(normalize_iv("").is_err());
    }
}
