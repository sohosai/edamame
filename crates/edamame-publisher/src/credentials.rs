//! live2025-server へ配信 URL と HLS 暗号鍵を登録する

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde_json::json;

/// 登録用 HTTP リクエスト 1 回あたりの制限時間
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 再試行までの待ち時間。
/// 再試行のたびに倍にし、上限まで延ばす
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// 登録に使う HTTP クライアントを作る
pub fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build the HTTP client: {error}"))
}

/// 登録先の URL を組み立てる
#[must_use]
pub fn credentials_url(base_url: &str) -> String {
    format!("{base_url}/internal/stream-credentials")
}

/// 登録する JSON を組み立てる
#[must_use]
pub fn credentials_body(channel_id: &str, live_url: &str, encryption_key_hex: &str) -> String {
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
fn register_once(client: &Client, url: &str, token: &str, body: &str) -> Result<(), RegisterError> {
    let response = match client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
    {
        Ok(response) => response,
        Err(error) => return Err(RegisterError::Retryable(format!("request failed: {error}"))),
    };
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let message = format!(
        "status {status} {}",
        response.text().unwrap_or_default().trim()
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
pub fn register_until_success(
    client: &Client,
    url: &str,
    token: &str,
    body: &str,
    channel_id: &str,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut delay = RETRY_MIN;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("stream credentials are not registered".to_owned());
        }
        match register_once(client, url, token, body) {
            Ok(()) => return Ok(()),
            Err(RegisterError::Retryable(message)) => {
                println!(
                    "channel={channel_id} retrying stream credentials registration: {message}"
                );
                sleep_interruptible(delay, stop);
                delay = (delay * 2).min(RETRY_MAX);
            }
            Err(RegisterError::Fatal(message)) => return Err(message),
        }
    }
}

/// 停止要求を確認しながら待つ
fn sleep_interruptible(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_credentials_url() {
        assert_eq!(
            credentials_url("https://live.example.test"),
            "https://live.example.test/internal/stream-credentials"
        );
    }

    #[test]
    fn builds_credentials_body() {
        let body = credentials_body(
            "uni",
            "https://live.example.test/channels/uni/master.m3u8",
            "0123456789abcdef0123456789abcdef",
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["channel_id"], "uni");
        assert_eq!(
            parsed["live_url"],
            "https://live.example.test/channels/uni/master.m3u8"
        );
        assert_eq!(parsed["encryption_key"], "0123456789abcdef0123456789abcdef");
    }
}
