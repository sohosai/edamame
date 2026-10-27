//! ImageFlux を fallback 配信として live2025-server へ登録する

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::json;
use tokio::sync::watch;

/// 登録用 HTTP リクエスト 1 回あたりの制限時間
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 再試行までの待ち時間。再試行のたびに倍にし、上限まで延ばす
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// 登録に使う HTTP クライアントを作る
pub fn client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("fallback 登録用の HTTP クライアントを作れません")
}

/// 登録先の URL を組み立てる
pub fn endpoint(base_url: &str, metric: u32) -> String {
    format!(
        "{}/internal/stream-credentials?metric={metric}",
        base_url.trim().trim_end_matches('/')
    )
}

/// 登録する JSON を組み立てる
pub fn body(channel_id: &str, live_url: &str, encryption_key_hex: &str) -> String {
    // 値のエスケープは serde_json に任せる
    json!({
        "channel_id": channel_id,
        "live_url": live_url,
        "encryption_key": encryption_key_hex,
    })
    .to_string()
}

/// 登録の結果
enum RegisterError {
    /// 時間をおいて再試行する
    Retryable(String),
    /// 設定を直さないと登録できない
    Fatal(String),
}

/// 1 回だけ登録する
async fn register_once(
    client: &Client,
    endpoint: &str,
    token: &str,
    body: &str,
) -> std::result::Result<(), RegisterError> {
    let response = match client
        .post(endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return Err(RegisterError::Retryable(format!("request failed: {error}"))),
    };
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let message = format!(
        "status {} {}",
        status.as_u16(),
        response.text().await.unwrap_or_default().trim()
    );
    // 400 と 401 は入力や認証の誤りなので、再試行しても登録できない
    if matches!(status.as_u16(), 400 | 401) {
        Err(RegisterError::Fatal(message))
    } else {
        Err(RegisterError::Retryable(message))
    }
}

/// 登録が成功するまで再試行する。
/// 停止要求を受けた場合や再試行できないエラーの場合は、登録を打ち切る
pub async fn register_until_success<F>(
    client: &Client,
    endpoint: &str,
    token: &str,
    body: &str,
    stop: &mut watch::Receiver<bool>,
    mut log: F,
) -> Result<()>
where
    F: FnMut(String),
{
    let mut delay = RETRY_MIN;
    loop {
        if *stop.borrow() {
            bail!("fallback 登録を中断しました");
        }
        match register_once(client, endpoint, token, body).await {
            Ok(()) => return Ok(()),
            Err(RegisterError::Retryable(message)) => {
                log(format!("fallback の登録を再試行します: {message}"));
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stop.changed() => bail!("fallback 登録を中断しました"),
                }
                delay = (delay * 2).min(RETRY_MAX);
            }
            Err(RegisterError::Fatal(message)) => bail!("{message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_fallback_endpoint() {
        assert_eq!(
            endpoint("https://live.example.test", 100_000),
            "https://live.example.test/internal/stream-credentials?metric=100000"
        );
        assert_eq!(
            endpoint("https://live.example.test/", 100_000),
            "https://live.example.test/internal/stream-credentials?metric=100000"
        );
    }

    #[test]
    fn builds_registration_body() {
        let body = body(
            "uni",
            "https://px.example.test/live.m3u8",
            "0123456789abcdef0123456789abcdef",
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["channel_id"], "uni");
        assert_eq!(parsed["live_url"], "https://px.example.test/live.m3u8");
        assert_eq!(parsed["encryption_key"], "0123456789abcdef0123456789abcdef");
    }
}
