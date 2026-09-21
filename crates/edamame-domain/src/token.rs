//! 認証トークンを検証する

use crate::error::EdamameError;

/// 認証トークンの形式を検証する
pub fn validate_auth_token(token: &str) -> Result<(), EdamameError> {
    if token.is_empty() {
        return Err(EdamameError::BadRequest(
            "auth token must not be empty".to_owned(),
        ));
    }
    // Authorization ヘッダーにそのまま載せられるよう、空白を除く印字可能な ASCII 文字だけを許可する
    if !token.chars().all(|character| character.is_ascii_graphic()) {
        return Err(EdamameError::BadRequest(
            "auth token must be printable ASCII without spaces".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_printable_ascii_token() {
        validate_auth_token("example-token_123").unwrap();
    }

    #[test]
    fn rejects_empty_token() {
        assert!(validate_auth_token("").is_err());
    }

    #[test]
    fn rejects_space_in_token() {
        assert!(validate_auth_token("secret token").is_err());
    }

    #[test]
    fn rejects_non_ascii_token() {
        assert!(validate_auth_token("トークン").is_err());
    }

    #[test]
    fn rejects_control_character_in_token() {
        assert!(validate_auth_token("secret\ntoken").is_err());
    }
}
