//! worker の登録と sync のループ

use std::sync::Arc;
use std::time::Duration;

use edamame_domain::{
    EdamameError, RegisterRequest, RegisterResponse, SegmentKey, SyncRequest, SyncResponse,
};

use crate::http::WorkerState;

enum SyncOutcome {
    Known(Vec<SegmentKey>),
    UnknownWorker,
}

/// 登録と sync を繰り返す
pub async fn run_sync_loop(state: Arc<WorkerState>, auth_token: Option<String>) {
    let mut registered = false;
    let mut interval_ms = 1000_u64;
    let mut fetch_task: Option<tokio::task::JoinHandle<()>> = None;
    // 取得が終わるまで controller が暗号化の指定を保持できるよう、取得中のセグメントを報告する
    let mut fetching: Vec<SegmentKey> = Vec::new();
    loop {
        if !registered {
            // 前の登録で始めた取得タスクがキャッシュ破棄後に古い内容を保存しないよう、先に止める
            stop_fetch(&mut fetch_task).await;
            fetching.clear();
            match register(&state, auth_token.as_deref()).await {
                Ok(received_interval_ms) => {
                    registered = true;
                    interval_ms = received_interval_ms;
                    // 前の登録で集めた索引を捨て、controller が指示するセグメントを取り直せるようにする
                    discard_cache(&state).await;
                }
                Err(error) => {
                    eprintln!("registration failed: {error}");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    continue;
                }
            }
        }
        // 取得タスクの終了後は fetching を空にする。
        // 取得済みのセグメントは、次の sync のキャッシュ一覧で報告する。
        if fetch_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(handle) = fetch_task.take() {
                let _ = handle.await;
            }
            fetching.clear();
        }
        match sync_once(&state, &fetching).await {
            Ok(SyncOutcome::Known(fetch_segments)) => {
                if !fetch_segments.is_empty() && fetch_task.is_none() {
                    fetching.clone_from(&fetch_segments);
                    let fetch_state = state.clone();
                    fetch_task = Some(tokio::spawn(async move {
                        fetch_missing_segments(fetch_state, fetch_segments).await;
                    }));
                }
            }
            Ok(SyncOutcome::UnknownWorker) => {
                registered = false;
                continue;
            }
            Err(error) => eprintln!("sync failed: {error}"),
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}

/// 進行中のセグメント取得を止める
async fn stop_fetch(fetch_task: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = fetch_task.take() {
        handle.abort();
        // 取得タスクがあとからキャッシュを書き換えないよう、中断が終わるまで待つ
        let _ = handle.await;
    }
}

/// キャッシュの索引とファイルを捨てて、controller から取り直せるようにする
async fn discard_cache(state: &WorkerState) {
    let paths = {
        let mut cache = state.cache.lock().await;
        cache.clear_all()
    };
    for path in paths {
        if let Err(error) = tokio::fs::remove_file(&path).await {
            eprintln!(
                "failed to remove cached segment {}: {error}",
                path.display()
            );
        }
    }
}

async fn register(state: &WorkerState, auth_token: Option<&str>) -> Result<u64, EdamameError> {
    let url = format!("{}/workers/{}", state.controller_url, state.worker_id);
    let request = state.client.put(&url).json(&RegisterRequest {
        public_url: state.public_url.clone(),
        weight: state.weight,
    });
    // 認証が必要なのは登録 PUT だけのため、指定時のみ Authorization ヘッダーを付ける
    let request = match auth_token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request
        .send()
        .await
        .map_err(|error| EdamameError::Internal(format!("registration request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(EdamameError::Internal(format!(
            "registration returned status {}",
            response.status()
        )));
    }
    let body: RegisterResponse = response.json().await.map_err(|error| {
        EdamameError::Internal(format!("registration response is invalid: {error}"))
    })?;
    if body.sync_interval_ms == 0 {
        return Err(EdamameError::Internal(
            "sync_interval_ms must be positive".to_owned(),
        ));
    }
    Ok(body.sync_interval_ms)
}

async fn sync_once(
    state: &WorkerState,
    fetching_segments: &[SegmentKey],
) -> Result<SyncOutcome, EdamameError> {
    let cached_segments = {
        let cache = state.cache.lock().await;
        cache.keys()
    };
    let url = format!("{}/workers/{}/sync", state.controller_url, state.worker_id);
    let response = state
        .client
        .post(&url)
        .json(&SyncRequest {
            cached_segments,
            fetching_segments: fetching_segments.to_vec(),
        })
        .send()
        .await
        .map_err(|error| EdamameError::Internal(format!("sync request failed: {error}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(SyncOutcome::UnknownWorker);
    }
    if !response.status().is_success() {
        return Err(EdamameError::Internal(format!(
            "sync returned status {}",
            response.status()
        )));
    }
    let body: SyncResponse = response
        .json()
        .await
        .map_err(|error| EdamameError::Internal(format!("sync response is invalid: {error}")))?;
    Ok(SyncOutcome::Known(body.fetch_segments))
}

async fn fetch_missing_segments(state: Arc<WorkerState>, segments: Vec<SegmentKey>) {
    for item in segments {
        let url = format!(
            "{}/segments/{}/{}/{}.ts",
            state.controller_url, item.channel_id, item.quality, item.sequence
        );
        match state.client.get(&url).send().await {
            Ok(response) if response.status().is_success() => match response.bytes().await {
                Ok(bytes) => {
                    let path = {
                        let cache = state.cache.lock().await;
                        cache.path_for(&item.channel_id, &item.quality, item.sequence)
                    };
                    let path = match path {
                        Ok(path) => path,
                        Err(error) => {
                            eprintln!(
                                "failed to resolve cached segment path {}/{}/{}: {error}",
                                item.channel_id, item.quality, item.sequence
                            );
                            continue;
                        }
                    };
                    if let Err(error) = crate::cache::write_atomic(&path, &bytes).await {
                        eprintln!(
                            "failed to store cached segment {}/{}/{}: {error}",
                            item.channel_id, item.quality, item.sequence
                        );
                        continue;
                    }
                    let over_limit = {
                        let mut cache = state.cache.lock().await;
                        cache.insert(&item.channel_id, &item.quality, item.sequence)
                    };
                    match over_limit {
                        Ok(paths) => {
                            for over_limit_path in &paths {
                                if let Err(error) = tokio::fs::remove_file(over_limit_path).await {
                                    eprintln!(
                                        "failed to remove cached segment over the limit {}: {error}",
                                        over_limit_path.display()
                                    );
                                }
                            }
                            // 保存したファイル自身が削除対象なら、索引はそのセグメントより新しい番号で埋まっている。
                            // 同じ channel と quality の索引とファイルを捨て、キャッシュ対象を取り直せるようにする。
                            if paths.contains(&path) {
                                let cleared = {
                                    let mut cache = state.cache.lock().await;
                                    cache.clear(&item.channel_id, &item.quality)
                                };
                                for cleared_path in &cleared {
                                    if let Err(error) = tokio::fs::remove_file(cleared_path).await {
                                        eprintln!(
                                            "failed to remove cached segment {}: {error}",
                                            cleared_path.display()
                                        );
                                    }
                                }
                            }
                        }
                        Err(error) => eprintln!(
                            "failed to index cached segment {}/{}/{}: {error}",
                            item.channel_id, item.quality, item.sequence
                        ),
                    }
                }
                Err(error) => eprintln!(
                    "failed to read segment body {}/{}/{}: {error}",
                    item.channel_id, item.quality, item.sequence
                ),
            },
            Ok(response) => eprintln!(
                "segment fetch returned status {} for {}/{}/{}",
                response.status(),
                item.channel_id,
                item.quality,
                item.sequence
            ),
            Err(error) => eprintln!(
                "segment fetch failed for {}/{}/{}: {error}",
                item.channel_id, item.quality, item.sequence
            ),
        }
    }
}
