//! peer の生存確認と leader の選出

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Leader,
    #[default]
    Follower,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub public_url: String,
    pub metric: u32,
    #[serde(default)]
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub public_url: String,
    pub metric: u32,
    #[serde(default)]
    pub role: Role,
    pub leader_url: Option<String>,
    pub members: Vec<Member>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub role: Role,
    pub public_url: Option<String>,
    pub metric: u32,
    pub leader_url: Option<String>,
}

struct Peer {
    member: Member,
    last_success: Option<Instant>,
    dead: bool,
}

pub(crate) struct ClusterInner {
    pub role: Role,
    pub leader_url: Option<String>,
    members: BTreeMap<String, Peer>,
    initialized: bool,
}

pub struct ClusterState {
    public_url: Option<String>,
    metric: u32,
    enabled: bool,
    auth_token: Option<String>,
    pub(crate) inner: RwLock<ClusterInner>,
    pub(crate) client: reqwest::Client,
    pub(crate) refresh: Notify,
    controller: Arc<ControllerState>,
}

impl ClusterState {
    pub fn new(
        config: &ControllerConfig,
        controller: Arc<ControllerState>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let enabled = config.cluster_enabled();
        let public_url = config
            .public_url
            .as_deref()
            .map(normalize_public_url)
            .transpose()?;
        Self::build(
            public_url,
            config.metric.unwrap_or(0),
            config.auth_token.clone(),
            enabled.then(|| config.cluster_seeds()).transpose()?,
            controller,
        )
    }

    pub(crate) fn standalone(controller: Arc<ControllerState>) -> Arc<Self> {
        Self::build(None, 0, None, None, controller).expect("HTTP client can be built")
    }

    fn build(
        public_url: Option<String>,
        metric: u32,
        auth_token: Option<String>,
        seeds: Option<Vec<String>>,
        controller: Arc<ControllerState>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let enabled = seeds.is_some();
        let mut members = BTreeMap::new();
        if let Some(seeds) = seeds {
            for public_url in seeds {
                eprintln!("cluster member added: {public_url}");
                members.insert(
                    public_url.clone(),
                    Peer {
                        member: Member {
                            public_url,
                            metric: 0,
                            role: Role::Follower,
                        },
                        last_success: None,
                        dead: false,
                    },
                );
            }
        }
        Ok(Arc::new(Self {
            public_url: public_url.clone(),
            metric,
            enabled,
            auth_token,
            inner: RwLock::new(ClusterInner {
                role: if enabled {
                    Role::Follower
                } else {
                    Role::Leader
                },
                leader_url: if enabled { None } else { public_url },
                members,
                initialized: !enabled,
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

    pub(crate) fn path_prefix(&self) -> String {
        self.public_url
            .as_deref()
            .map(|url| {
                reqwest::Url::parse(url)
                    .expect("validated public URL")
                    .path()
                    .trim_end_matches('/')
                    .to_owned()
            })
            .unwrap_or_default()
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

    fn snapshot(&self, inner: &ClusterInner) -> Heartbeat {
        Heartbeat {
            public_url: self.public_url.clone().expect("cluster has public URL"),
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

    pub async fn receive(&self, heartbeat: Heartbeat) -> Result<Heartbeat, EdamameError> {
        if !self.enabled {
            return Err(EdamameError::Unavailable("cluster is disabled".to_owned()));
        }
        let mut inner = self.inner.write().await;
        self.merge(&mut inner, heartbeat, Instant::now())?;
        self.elect(&mut inner, Instant::now()).await;
        Ok(self.snapshot(&inner))
    }

    fn merge(
        &self,
        inner: &mut ClusterInner,
        heartbeat: Heartbeat,
        now: Instant,
    ) -> Result<(), EdamameError> {
        let public_url = normalize_public_url(&heartbeat.public_url)?;
        // 間接的な報告では生存時刻と優先度を更新せず、古い報告による延命を防ぐ。
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
        if Some(&public_url) != self.public_url.as_ref() {
            self.add_member(
                inner,
                Member {
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

    fn add_member(&self, inner: &mut ClusterInner, member: Member) {
        if Some(&member.public_url) == self.public_url.as_ref()
            || inner.members.contains_key(&member.public_url)
        {
            return;
        }
        eprintln!("cluster member added: {}", member.public_url);
        inner.members.insert(
            member.public_url.clone(),
            Peer {
                member,
                last_success: None,
                dead: false,
            },
        );
    }

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
        let own_url = self.public_url.as_deref().expect("cluster has public URL");
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

    pub async fn run(self: Arc<Self>) {
        if !self.enabled {
            return;
        }
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
                let token = self.auth_token.clone().expect("cluster has token");
                requests.spawn(async move {
                    let response = client
                        .post(format!("{url}/internal/cluster/heartbeat"))
                        .bearer_auth(token)
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

fn select_leader<'a>(
    own_url: &'a str,
    metric: u32,
    peers: impl IntoIterator<Item = (&'a str, u32, Option<Instant>)>,
    now: Instant,
) -> &'a str {
    peers
        .into_iter()
        .filter(|(_, _, last)| last.is_some_and(|last| now.duration_since(last) < MEMBER_TIMEOUT))
        .map(|(url, metric, _)| (metric, url))
        .chain(std::iter::once((metric, own_url)))
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
        let cluster = ClusterState::build(
            Some("http://a.test".to_owned()),
            10,
            Some("token".to_owned()),
            Some(vec![]),
            controller,
        )
        .unwrap();
        let now = Instant::now();
        let mut inner = cluster.inner.write().await;
        let peer = |url: &str, metric| Member {
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
    fn selects_largest_live_metric_at_timeout_boundary() {
        let now = Instant::now();
        // 3 秒未満の応答だけを生存とし、未接触の peer は選ばない。
        for (age, expected) in [(2999, "http://b"), (3000, "http://a"), (3001, "http://a")] {
            assert_eq!(
                select_leader(
                    "http://a",
                    1,
                    [
                        ("http://b", 2, Some(now - Duration::from_millis(age))),
                        ("http://c", 9, None)
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
            select_leader("http://a", 3, [("http://b", 2, Some(now))], now),
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
}
