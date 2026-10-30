//! peer の生存確認と leader の選出

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edamame_domain::{EdamameError, normalize_public_url};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinSet;

use crate::{ControllerConfig, ControllerState};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MEMBER_TIMEOUT: Duration = Duration::from_secs(3);

/// cluster の起動直後など、leader が未確定の間も follower として表す。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Leader,
    #[default]
    Follower,
}

/// heartbeat で交換する peer の情報。生存時刻は共有しない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub public_url: String,
    pub metric: u32,
    #[serde(default)]
    pub role: Role,
}

/// peer の発見と生存確認に使う、要求と応答に共通の情報。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub public_url: String,
    pub metric: u32,
    #[serde(default)]
    pub role: Role,
    pub leader_url: Option<String>,
    pub members: Vec<Peer>,
}

/// 監視先自身の状態。leader が未確定でも取得できる。
#[derive(Debug, Serialize)]
pub struct Health {
    pub role: Role,
    pub public_url: String,
    pub metric: u32,
    pub leader_url: Option<String>,
}

/// 直接の疎通結果を保存する監視状態。ほかの peer とは共有しない。
struct PeerState {
    member: Peer,
    /// 未接触の peer は選出対象にしないため、成功するまでは None にする。
    last_success: Option<Instant>,
    /// 死亡ログの重複と復帰の検出に使い、生存判定は最終成功時刻で行う。
    dead: bool,
}

/// 役割の切り替えと HTTP 処理が競合しないよう、選出結果と peer 表をまとめて保護する。
pub(crate) struct ClusterInner {
    pub role: Role,
    pub leader_url: Option<String>,
    members: BTreeMap<String, PeerState>,
    /// 初回の heartbeat 送信が一巡するまで、自分を leader にしないためのフラグ。
    initialized: bool,
}

/// HTTP 層が選出結果を参照できるよう、peer の監視状態を共有する。
pub struct ClusterState {
    public_url: String,
    metric: u32,
    pub(crate) auth_token: Arc<str>,
    pub(crate) inner: RwLock<ClusterInner>,
    pub(crate) client: reqwest::Client,
    pub(crate) refresh: Notify,
    controller: Arc<ControllerState>,
}

impl ClusterState {
    /// peer がいない場合も同じ選出処理を使い、初回の生存確認待ちで開始する。
    pub fn new(
        config: &ControllerConfig,
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
                    member: Peer {
                        public_url,
                        metric: 0,
                        role: Role::Follower,
                    },
                    last_success: None,
                    dead: false,
                },
            );
        }
        Ok(Arc::new(Self {
            public_url,
            metric: config.metric,
            auth_token: Arc::from(config.auth_token.as_str()),
            inner: RwLock::new(ClusterInner {
                role: Role::Follower,
                leader_url: None,
                members,
                initialized: false,
            }),
            // peer が別の follower を指しても、認証付き PUT を redirect で送らない。
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            refresh: Notify::new(),
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
            role: inner.role,
            public_url: self.public_url.clone(),
            metric: self.metric,
            leader_url: inner.leader_url.clone(),
        }
    }

    /// 生存時刻を peer 間で混同しないよう、共有する情報だけを送信用に切り出す。
    fn snapshot(&self, inner: &ClusterInner) -> Heartbeat {
        Heartbeat {
            public_url: self.public_url.clone(),
            metric: self.metric,
            role: inner.role,
            leader_url: inner.leader_url.clone(),
            members: inner
                .members
                .values()
                .map(|peer| peer.member.clone())
                .collect(),
        }
    }

    /// 送信元の生存確認と選出を反映し、更新後の状態を heartbeat の応答にする。
    pub async fn receive(&self, heartbeat: Heartbeat) -> Result<Heartbeat, EdamameError> {
        let mut inner = self.inner.write().await;
        self.merge(&mut inner, heartbeat, Instant::now())?;
        self.elect(&mut inner, Instant::now()).await;
        Ok(self.snapshot(&inner))
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
                    metric: heartbeat.metric,
                    role: heartbeat.role,
                },
            );
            let peer = inner
                .members
                .get_mut(&public_url)
                .expect("member was inserted");
            if peer.dead {
                eprintln!("cluster member recovered: {public_url}");
            }
            peer.member.metric = heartbeat.metric;
            peer.member.role = heartbeat.role;
            peer.last_success = Some(now);
            peer.dead = false;
        }
        Ok(())
    }

    /// 自分と既知の URL を除いて登録し、直接疎通するまでは選出対象にしない。
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

    /// 初回の生存確認が済んだら役割を確定し、降格時は配信状態を捨てる。
    async fn elect(&self, inner: &mut ClusterInner, now: Instant) {
        for peer in inner.members.values_mut() {
            let dead = peer
                .last_success
                .is_none_or(|last| now.duration_since(last) >= MEMBER_TIMEOUT);
            if dead && !peer.dead {
                eprintln!("cluster member dead: {}", peer.member.public_url);
                peer.dead = true;
            }
        }
        if !inner.initialized {
            return;
        }
        let own_url = self.public_url.as_str();
        let leader = select_leader(
            own_url,
            self.metric,
            inner.members.values().map(|peer| {
                (
                    peer.member.public_url.as_str(),
                    peer.member.metric,
                    peer.last_success,
                )
            }),
            now,
        );
        let role = if leader == own_url {
            Role::Leader
        } else {
            Role::Follower
        };
        if inner.role != role {
            // HTTP 層の読み取りロックが解放されてから消し、処理中の PUT の書き戻しを防ぐ。
            if role == Role::Follower {
                self.controller.clear().await;
            }
            eprintln!(
                "cluster role changed: {:?} -> {:?}, leader={leader}",
                inner.role, role
            );
        }
        inner.role = role;
        inner.leader_url = Some(leader.to_owned());
    }

    /// TLS を終端できなくなった機体を cluster から外す
    ///
    /// 自身の公開 URL へ接続できない状態で leader を続けると、client から到達できない
    /// leader に採番や保存を任せることになる。役割と保持する状態を捨て、復旧後に
    /// 改めて選出へ参加させる。
    pub async fn leave_cluster(&self) {
        let mut inner = self.inner.write().await;
        if inner.role == Role::Leader {
            self.controller.clear().await;
        }
        inner.role = Role::Follower;
        inner.leader_url = None;
        inner.initialized = false;
        eprintln!("cluster membership reset because tls is unavailable");
    }

    /// 1 秒ごと、または中継失敗の通知時に全 peer を確認して選び直す。
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.refresh.notified() => {}
            }
            let (heartbeat, urls) = {
                let inner = self.inner.read().await;
                (
                    self.snapshot(&inner),
                    inner.members.keys().cloned().collect::<Vec<_>>(),
                )
            };
            let mut requests = JoinSet::new();
            for url in urls {
                let client = self.client.clone();
                let heartbeat = heartbeat.clone();
                let token = self.auth_token.clone();
                requests.spawn(async move {
                    let response = client
                        .post(format!("{url}/internal/cluster/heartbeat"))
                        .bearer_auth(token.as_ref())
                        .json(&heartbeat)
                        .timeout(HEARTBEAT_INTERVAL)
                        .send()
                        .await?
                        .error_for_status()?;
                    let heartbeat = response.json::<Heartbeat>().await?;
                    Ok::<_, reqwest::Error>((url, heartbeat))
                });
            }
            while let Some(result) = requests.join_next().await {
                if let Ok(Ok((url, heartbeat))) = result {
                    // 応答元と宣言した URL が違う場合は、その peer の生存根拠にしない。
                    if normalize_public_url(&heartbeat.public_url).ok().as_ref() == Some(&url) {
                        let mut inner = self.inner.write().await;
                        if let Err(error) = self.merge(&mut inner, heartbeat, Instant::now()) {
                            eprintln!("invalid cluster heartbeat: {error}");
                        }
                    }
                }
            }
            let mut inner = self.inner.write().await;
            inner.initialized = true;
            self.elect(&mut inner, Instant::now()).await;
        }
    }
}

/// 直接疎通から 3 秒未満の peer と自分の中で、metric が最小の機体を選ぶ。
/// metric が同じ場合は URL が最大の機体を選ぶ。
fn select_leader<'a>(
    own_url: &'a str,
    metric: u32,
    peers: impl IntoIterator<Item = (&'a str, u32, Option<Instant>)>,
    now: Instant,
) -> &'a str {
    peers
        .into_iter()
        .filter(|(_, _, last)| last.is_some_and(|last| now.duration_since(last) < MEMBER_TIMEOUT))
        .map(|(url, metric, _)| (Reverse(metric), url))
        .chain(std::iter::once((Reverse(metric), own_url)))
        .max()
        .expect("self is always alive")
        .1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merges_members_without_gossip_extending_liveness() {
        let controller = ControllerState::new(crate::CanonicalStore::new(1024), 6, 100, 1000, 0, 1);
        let config = ControllerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            storage_capacity: 1024,
            playlist_segments: 6,
            sync_interval_ms: 100,
            sync_timeout_ms: 1000,
            publish_wait_ms: Some(0),
            external_min_percent: 1,
            cors_origin: vec![],
            public_url: "http://a.test".to_owned(),
            metric: 50,
            auth_token: "token".to_owned(),
            seed: vec![],
            tls_cert: None,
            tls_key: None,
            acme: false,
            acme_directory: None,
            acme_cache_dir: None,
            acme_contact: None,
        };
        let cluster = ClusterState::new(&config, controller).unwrap();
        let now = Instant::now();
        let mut inner = cluster.inner.write().await;
        let peer = |url: &str, metric| Peer {
            public_url: url.to_owned(),
            metric,
            role: Role::Leader,
        };
        let heartbeat = |url: &str, metric, members| Heartbeat {
            public_url: url.to_owned(),
            metric,
            role: Role::Leader,
            leader_url: Some(url.to_owned()),
            members,
        };
        cluster
            .merge(
                &mut inner,
                heartbeat(
                    "http://b.test/",
                    20,
                    vec![
                        peer("http://c.test/", 30),
                        peer("http://c.test", 30),
                        peer("http://a.test/", 10),
                    ],
                ),
                now,
            )
            .unwrap();
        assert_eq!(inner.members.len(), 2);
        assert_eq!(inner.members["http://b.test"].last_success, Some(now));
        assert!(inner.members["http://c.test"].last_success.is_none());
        cluster
            .merge(
                &mut inner,
                heartbeat("http://c.test", 40, vec![peer("http://b.test", 999)]),
                now + Duration::from_secs(4),
            )
            .unwrap();
        // 別の peer の報告で古い生存時刻や優先度を上書きしない。
        assert_eq!(inner.members["http://b.test"].last_success, Some(now));
        assert_eq!(inner.members["http://b.test"].member.metric, 20);
        assert_eq!(inner.members["http://c.test"].member.metric, 40);
        cluster
            .elect(&mut inner, now + Duration::from_secs(4))
            .await;
        assert!(
            inner.leader_url.is_none(),
            "初回の送信完了までは leader を確定しない"
        );
        assert!(inner.members["http://b.test"].dead);
        assert_eq!(inner.members.len(), 2, "死亡した member も表に残す");
        inner.initialized = true;
        cluster
            .elect(&mut inner, now + Duration::from_secs(4))
            .await;
        assert_eq!(inner.leader_url.as_deref(), Some("http://c.test"));
        let count = inner.members.len();
        assert!(
            cluster
                .merge(
                    &mut inner,
                    heartbeat("http://d.test", 50, vec![peer("/invalid", 90)]),
                    now
                )
                .is_err()
        );
        assert_eq!(
            inner.members.len(),
            count,
            "不正な報告は部分的にも取り込まない"
        );
    }

    #[test]
    fn selects_smallest_live_metric_at_timeout_boundary() {
        let now = Instant::now();
        // 3 秒未満の応答だけを生存とし、未接触の peer は選ばない。
        for (age, expected) in [(2999, "http://b"), (3000, "http://a"), (3001, "http://a")] {
            assert_eq!(
                select_leader(
                    "http://a",
                    9,
                    [
                        ("http://b", 2, Some(now - Duration::from_millis(age))),
                        ("http://c", 1, None)
                    ],
                    now
                ),
                expected
            );
        }
    }

    #[test]
    fn selects_self_and_breaks_ties_by_public_url() {
        let now = Instant::now();
        assert_eq!(
            select_leader("http://a", 1, [("http://b", 2, Some(now))], now),
            "http://a"
        );
        assert_eq!(
            select_leader("http://a", 2, [("http://b", 2, Some(now))], now),
            "http://b"
        );
        assert_eq!(
            select_leader("http://z", 2, [("http://b", 2, Some(now))], now),
            "http://z"
        );
    }

    #[tokio::test]
    async fn resets_membership_when_tls_becomes_unavailable() {
        let controller = ControllerState::new(crate::CanonicalStore::new(1024), 6, 100, 1000, 0, 1);
        let config = ControllerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            storage_capacity: 1024,
            playlist_segments: 6,
            sync_interval_ms: 100,
            sync_timeout_ms: 1000,
            publish_wait_ms: Some(0),
            external_min_percent: 1,
            cors_origin: vec![],
            public_url: "http://a.test".to_owned(),
            metric: 10,
            auth_token: "token".to_owned(),
            seed: vec![],
            tls_cert: None,
            tls_key: None,
            acme: false,
            acme_directory: None,
            acme_cache_dir: None,
            acme_contact: None,
        };
        let cluster = ClusterState::new(&config, controller).unwrap();
        {
            let mut inner = cluster.inner.write().await;
            inner.role = Role::Leader;
            inner.leader_url = Some("http://a.test".to_owned());
            inner.initialized = true;
        }
        cluster.leave_cluster().await;
        let inner = cluster.inner.read().await;
        assert_eq!(inner.role, Role::Follower);
        assert_eq!(inner.leader_url, None);
        assert!(!inner.initialized);
    }
}
