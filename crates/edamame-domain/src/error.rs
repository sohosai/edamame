//! HTTP のステータスコードに対応する共通エラー型を定義する

use std::fmt;

/// controller と worker が共有するエラーを表す
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdamameError {
    /// 入力が不正である (400)
    BadRequest(String),
    /// 対象が存在しない (404)
    NotFound(String),
    /// 既存のデータと競合する (409)
    Conflict(String),
    /// 送られたデータが大きすぎる (413)
    PayloadTooLarge(String),
    /// プレイリストを生成できない (422)
    Unprocessable(String),
    /// 配信できる worker がない (503)
    Unavailable(String),
    /// 予期しない内部エラーが発生した (500)
    Internal(String),
}

impl EdamameError {
    /// このエラーに対応する HTTP ステータスコードを返す
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::PayloadTooLarge(_) => 413,
            Self::Unprocessable(_) => 422,
            Self::Unavailable(_) => 503,
            Self::Internal(_) => 500,
        }
    }

    /// エラーの説明を返す
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::PayloadTooLarge(message)
            | Self::Unprocessable(message)
            | Self::Unavailable(message)
            | Self::Internal(message) => message,
        }
    }
}

impl fmt::Display for EdamameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message(), self.status_code())
    }
}

impl std::error::Error for EdamameError {}
