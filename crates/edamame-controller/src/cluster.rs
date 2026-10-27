//! peer の生存確認と worker registry の共有

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edamame_domain::{EdamameError, normalize_public_url, now_ms};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::state::{ControllerState, WorkerSnapshot};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MEMBER_TIMEOUT: Duration = Duration::from_secs(3);

/// heartbeat で交換する peer の情報。生存時刻は共有しない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub public_url: String,
}

/// peer の発見と worker registry の共有に使う、要求と応答に共通の情報。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub public_url: String,
    pub members: Vec<Peer>,
    /// この controller が知っている worker の状態。受信側で自分の registry に取り込む。
    #[serde(default)]
    pub workers: Vec<WorkerSnapshot>,
}

/// 監視先自身の状態。
#[derive(Debug, Serialize)]
pub struct Health {
    pub public_url: String,
    pub members: Vec<String>,
}

/// 直接の疎通結果を保存する監視状態。ほかの peer とは共有しない。
struct PeerState {
    member: Peer,
    last_success: Option<Instant>,
    dead: bool,
}

/// peer 表を保護する。
pub(crate) struct ClusterInner {
    members: BTreeMap<String, PeerState>,
}

/// HTTP 層が peer 表と worker registry を参照できるよう、状態を共有する。
pub struct ClusterState {
    public_url: String,
    pub(crate) auth_token: Arc<str>,
    pub(crate) inner: RwLock<ClusterInner>,
    pub(crate) client: reqwest::Client,
    controller: Arc<ControllerState>,
}

impl ClusterState {
    /// seed を初期 peer として登録する。leader は選ばない。
    pub fn new(
        config: &crate::ControllerConfig,
        controller: Arc<ControllerState>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        config.validate()?;
        let public_url = normalize_public_url(&config.public_url)?;
        let mut members = BTreeMap::new();
        for public_url in config.cluster_seeds()? {
            eprintln!("cluster member added: {public_url}");
            members.insert(
                public_url.clone(),
                PeerState {
                    member: Peer { public_url },
                    last_success: None,
                    dead: false,
                },
            );
        }
        Ok(Arc::new(Self {
            public_url,
            auth_token: Arc::from(config.auth_token.as_str()),
            inner: RwLock::new(ClusterInner { members }),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            controller,
        }))
    }

    /// API の配置と転送先の組み立てに使う、公開 URL の path 接頭辞。
    pub(crate) fn path_prefix(&self) -> String {
        reqwest::Url::parse(&self.public_url)
            .expect("validated public URL")
            .path()
            .trim_end_matches('/')
            .to_owned()
    }

    pub async fn health(&self) -> Health {
        let inner = self.inner.read().await;
        Health {
            public_url: self.public_url.clone(),
            members: inner.members.keys().cloned().collect(),
        }
    }

    /// 送信する heartbeat に、自分の worker registry を含める。
    async fn snapshot(&self) -> Heartbeat {
        let members = {
            let inner = self.inner.read().await;
            inner
                .members
                .values()
                .map(|peer| peer.member.clone())
                .collect()
        };
        let workers = self.controller.worker_snapshots().await;
        Heartbeat {
            public_url: self.public_url.clone(),
            members,
            workers,
        }
    }

    /// 受信した heartbeat の member と worker registry を反映し、自分の状態を返す。
    pub async fn receive(&self, heartbeat: Heartbeat) -> Result<Heartbeat, EdamameError> {
        {
            let mut inner = self.inner.write().await;
            self.merge(&mut inner, heartbeat.clone(), Instant::now())?;
        }
        self.apply_workers(heartbeat.workers).await;
        Ok(self.snapshot().await)
    }

    /// 間接的な報告で延命しないよう、送信元の生存情報と未登録の接触先だけを取り込む。
    fn merge(
        &self,
        inner: &mut ClusterInner,
        heartbeat: Heartbeat,
        now: Instant,
    ) -> Result<(), EdamameError> {
        let public_url = normalize_public_url(&heartbeat.public_url)?;
        let members = heartbeat
            .members
            .into_iter()
            .map(|mut member| {
                member.public_url = normalize_public_url(&member.public_url)?;
                Ok(member)
            })
            .collect::<Result<Vec<_>, EdamameError>>()?;
        for member in members {
            self.add_member(inner, member);
        }
        if public_url != self.public_url {
            self.add_member(
                inner,
                Peer {
                    public_url: public_url.clone(),
                },
            );
            let peer = inner
                .members
                .get_mut(&public_url)
                .expect("member was inserted");
            if peer.dead {
                eprintln!("cluster member recovered: {public_url}");
            }
            peer.last_success = Some(now);
            peer.dead = false;
        }
        Ok(())
    }

    /// 自分と既知の URL を除いて登録し、直接疎通するまでは送信対象にしない。
    fn add_member(&self, inner: &mut ClusterInner, member: Peer) {
        if member.public_url == self.public_url || inner.members.contains_key(&member.public_url) {
            return;
        }
        eprintln!("cluster member added: {}", member.public_url);
        inner.members.insert(
            member.public_url.clone(),
            PeerState {
                member,
                last_success: None,
                dead: false,
            },
        );
    }

    /// heartbeat で届いた worker の状態を controller に取り込む。
    async fn apply_workers(&self, workers: Vec<WorkerSnapshot>) {
        for worker in workers {
            if let Err(error) = self
                .controller
                .apply_worker_snapshot(worker, now_ms())
                .await
            {
                eprintln!("worker snapshot rejected: {error}");
            }
        }
    }

    /// 1 秒ごとに全 peer と heartbeat を交換し、member と worker registry を更新する。
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let heartbeat = self.snapshot().await;
            let urls = {
                let inner = self.inner.read().await;
                inner.members.keys().cloned().collect::<Vec<_>>()
            };
            let mut requests = JoinSet::new();
            for url in urls {
                let client = self.client.clone();
                let heartbeat = heartbeat.clone();
                let token = self.auth_token.clone();
                requests.spawn(async move {
                    let result = async {
                        let response = client
                            .post(format!("{url}/internal/cluster/heartbeat"))
                            .bearer_auth(token.as_ref())
                            .json(&heartbeat)
                            .timeout(HEARTBEAT_INTERVAL)
                            .send()
                            .await?
                            .error_for_status()?;
                        let heartbeat = response.json::<Heartbeat>().await?;
                        Ok::<_, reqwest::Error>((url.clone(), heartbeat))
                    }
                    .await;
                    (url, result)
                });
            }
            let mut dead_urls = Vec::new();
            while let Some(result) = requests.join_next().await {
                match result {
                    Ok((url, Ok((reported_url, heartbeat)))) => {
                        // 応答元と宣言した URL が違う場合は、その peer の生存根拠にしない。
                        if reported_url == url
                            && normalize_public_url(&heartbeat.public_url).ok().as_ref()
                                == Some(&url)
                        {
                            {
                                let mut inner = self.inner.write().await;
                                if let Err(error) =
                                    self.merge(&mut inner, heartbeat.clone(), Instant::now())
                                {
                                    eprintln!("invalid cluster heartbeat: {error}");
                                }
                            }
                            self.apply_workers(heartbeat.workers).await;
                        }
                    }
                    Ok((url, Err(error))) => {
                        eprintln!("cluster heartbeat failed: {url}: {error}");
                        dead_urls.push(url);
                    }
                    Err(error) => {
                        eprintln!("cluster heartbeat task failed: {error}");
                    }
                }
            }
            if !dead_urls.is_empty() {
                let mut inner = self.inner.write().await;
                for url in dead_urls {
                    if let Some(peer) = inner.members.get_mut(&url) {
                        let dead = peer
                            .last_success
                            .is_none_or(|last| now_elapsed(last) >= MEMBER_TIMEOUT);
                        if dead && !peer.dead {
                            eprintln!("cluster member dead: {url}");
                            peer.dead = true;
                        }
                    }
                }
            }
        }
    }
}

fn now_elapsed(last: Instant) -> Duration {
    Instant::now().duration_since(last)
}
