//! worker の TLS 配信

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use edamame_domain::EdamameError;
use rustls::pki_types::pem::PemObject;
use rustls_acme::AcmeConfig;
use rustls_acme::caches::DirCache;
use tokio_stream::StreamExt;

/// 再開用に保持するセッションの数
///
/// 視聴者ごとに接続が作られるため、既定の 256 では同時接続数を下回る。保持できなかった
/// 接続は毎回ハンドシェイクの署名と鍵交換を行うことになる
const SESSION_CACHE_SIZE: usize = 16384;

/// TLS の暗号実装をプロセスに 1 度だけ登録する
fn install_crypto_provider() {
    // すでに登録されている場合もエラーにしない
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 配信に共通の TLS 設定を加える
///
/// 既定の rustls はチケットを発行しないため、client が前回のセッションを提示しても
/// 再開できない。配信では同じ client が繰り返し接続するので、再開できないと
/// 接続のたびに証明書の署名と鍵交換をやり直すことになる
fn apply_serving_defaults(config: &mut rustls::ServerConfig) -> Result<(), EdamameError> {
    // 配信の応答は 1 本が数 MB のセグメント 1 つで、多重化する相手がいない。h2 を
    // 広告すると、そのセグメントをフレーム分割してフロー制御の更新を挟むことになる
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.ticketer = rustls::crypto::ring::Ticketer::new()
        .map_err(|error| EdamameError::Internal(format!("create tls ticketer failed: {error}")))?;
    config.session_storage = rustls::server::ServerSessionMemoryCache::new(SESSION_CACHE_SIZE);
    Ok(())
}

/// 配信に使う rustls の crypto provider を作る
fn crypto_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    std::sync::Arc::new(rustls::crypto::ring::default_provider())
}

/// PEM ファイルから TLS 設定を組み立てる
///
/// axum-server の `from_pem_file` は既定の rustls 設定をそのまま使うため、
/// セッション再開を有効にした設定を自前で作る
async fn load_server_config(
    cert: &Path,
    key: &Path,
) -> Result<std::sync::Arc<rustls::ServerConfig>, EdamameError> {
    let cert_pem = tokio::fs::read(cert)
        .await
        .map_err(|error| EdamameError::Internal(format!("read tls certificate failed: {error}")))?;
    let key_pem = tokio::fs::read(key)
        .await
        .map_err(|error| EdamameError::Internal(format!("read tls key failed: {error}")))?;
    let certs = rustls::pki_types::CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            EdamameError::Internal(format!("parse tls certificate failed: {error}"))
        })?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|error| EdamameError::Internal(format!("parse tls key failed: {error}")))?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| EdamameError::Internal(format!("build tls config failed: {error}")))?;
    apply_serving_defaults(&mut config)?;
    Ok(std::sync::Arc::new(config))
}

/// ACME の証明書リゾルバを使う TLS 設定を組み立てる
///
/// rustls-acme の `default_rustls_config` はセッション再開が無効なため、
/// 同じ設定を自前で作り、配信に必要な設定を加える
fn acme_server_config(
    resolver: std::sync::Arc<rustls_acme::ResolvesServerCertAcme>,
) -> Result<std::sync::Arc<rustls::ServerConfig>, EdamameError> {
    let mut config = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| EdamameError::Internal(format!("build tls config failed: {error}")))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    apply_serving_defaults(&mut config)?;
    Ok(std::sync::Arc::new(config))
}

/// PEM ファイルの証明書を読み込み、更新の監視を始める
pub async fn static_config(cert: PathBuf, key: PathBuf) -> Result<RustlsConfig, EdamameError> {
    install_crypto_provider();
    let config = RustlsConfig::from_config(load_server_config(&cert, &key).await?);
    // 証明書ファイルが更新されたら読み直し、再起動を不要にする
    spawn_reload(config.clone(), cert, key);
    Ok(config)
}

/// 読み込み済みの証明書で TLS 配信する
pub async fn serve_static(
    listener: TcpListener,
    app: Router,
    config: RustlsConfig,
) -> Result<(), EdamameError> {
    axum_server::from_tcp_rustls(listener, config)
        .map_err(|error| EdamameError::Internal(format!("tls listener failed: {error}")))?
        .serve(app.into_make_service())
        .await
        .map_err(|error| EdamameError::Internal(format!("tls server failed: {error}")))
}

/// ACME で証明書を取得して TLS 配信する
///
/// 配信中の証明書を client と同じ手順で確認し、有効な間だけ `valid` で true を通知する
pub async fn serve_acme(
    listener: TcpListener,
    app: Router,
    domain: &str,
    directory: &str,
    cache_dir: PathBuf,
    contact: Option<&str>,
    valid: tokio::sync::watch::Sender<bool>,
) -> Result<(), EdamameError> {
    install_crypto_provider();
    let addr = listener
        .local_addr()
        .map_err(|error| EdamameError::Internal(format!("read tls address failed: {error}")))?;
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(|error| {
            EdamameError::Internal(format!("create acme cache dir failed: {error}"))
        })?;
    // アカウント鍵と証明書の秘密鍵が入るため、同じホストの他の利用者から読めないようにする
    restrict_to_owner(&cache_dir).await?;
    let contacts = contact
        .map(|contact| vec![format!("mailto:{contact}")])
        .unwrap_or_default();
    let mut state = AcmeConfig::new([domain])
        .contact(contacts)
        .cache(DirCache::new(cache_dir.clone()))
        .directory(directory)
        .state();
    let acceptor = state.axum_acceptor(acme_server_config(state.resolver())?);
    // rustls-acme のアクセプタにはハンドシェイクの時間制限がないため、資源を使い続けないようにする
    let acceptor = HandshakeTimeoutAcceptor {
        inner: acceptor,
        timeout: Duration::from_secs(10),
    };
    let probe_config = probe_client_config();
    {
        // 証明書が入れ替わったら、すぐに有効性を確かめて通知する
        let valid = valid.clone();
        let domain = domain.to_owned();
        let probe_config = probe_config.clone();
        tokio::spawn(async move {
            while let Some(event) = state.next().await {
                match event {
                    Ok(event) => {
                        println!("acme event: {event:?}");
                        if matches!(
                            event,
                            rustls_acme::EventOk::DeployedNewCert
                                | rustls_acme::EventOk::DeployedCachedCert
                                | rustls_acme::EventOk::CertCacheStore
                        ) {
                            let valid_now =
                                probe_certificate_with_retry(&domain, addr, &probe_config).await;
                            let _ = valid.send(valid_now);
                        }
                    }
                    Err(error) => eprintln!("acme error: {error:?}"),
                }
            }
            eprintln!("acme certificate management stopped");
        });
    }
    {
        // 更新に失敗して期限切れになったら、配信先から外れるよう通知を続ける
        let domain = domain.to_owned();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await;
            loop {
                interval.tick().await;
                let valid_now = probe_certificate(&domain, addr, &probe_config).await;
                let _ = valid.send(valid_now);
            }
        });
    }
    axum_server::from_tcp(listener)
        .map_err(|error| EdamameError::Internal(format!("tls listener failed: {error}")))?
        .acceptor(acceptor)
        .serve(app.into_make_service())
        .await
        .map_err(|error| EdamameError::Internal(format!("tls server failed: {error}")))
}

/// 配信中の証明書を、client と同じ検証で確かめる
async fn probe_certificate(
    domain: &str,
    addr: SocketAddr,
    config: &std::sync::Arc<rustls::ClientConfig>,
) -> bool {
    let Ok(server_name) = rustls::pki_types::ServerName::try_from(domain.to_owned()) else {
        return false;
    };
    let Ok(stream) = tokio::net::TcpStream::connect(addr).await else {
        return false;
    };
    let connector = tokio_rustls::TlsConnector::from(config.clone());
    matches!(
        tokio::time::timeout(
            Duration::from_secs(10),
            connector.connect(server_name, stream)
        )
        .await,
        Ok(Ok(_))
    )
}

/// サーバーが接続を受け付けるまで、短い間隔で証明書を確かめる
async fn probe_certificate_with_retry(
    domain: &str,
    addr: SocketAddr,
    config: &std::sync::Arc<rustls::ClientConfig>,
) -> bool {
    for _ in 0..10 {
        if probe_certificate(domain, addr, config).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// TLS ハンドシェイクが終わらない接続で資源を使い続けないようにする
#[derive(Clone)]
struct HandshakeTimeoutAcceptor<A> {
    inner: A,
    timeout: Duration,
}

impl<I, S, A> axum_server::accept::Accept<I, S> for HandshakeTimeoutAcceptor<A>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
    A: axum_server::accept::Accept<I, S> + Clone + Send + Sync + 'static,
    A::Future: Send + 'static,
{
    type Stream = A::Stream;
    type Service = A::Service;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<(A::Stream, A::Service)>> + Send>,
    >;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let future = self.inner.accept(stream, service);
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, future).await {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "tls handshake timed out",
                )),
            }
        })
    }
}

/// client と同じ root 証明書で検証する設定を作る
fn probe_client_config() -> std::sync::Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    client_config(roots)
}

/// 指定した root 証明書で検証する設定を作る
fn client_config(roots: rustls::RootCertStore) -> std::sync::Arc<rustls::ClientConfig> {
    let mut config = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    std::sync::Arc::new(config)
}

/// 所有者だけがディレクトリを読み書きできるようにする
async fn restrict_to_owner(path: &Path) -> Result<(), EdamameError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| {
                EdamameError::Internal(format!("set acme cache permissions failed: {error}"))
            })?;
    }
    Ok(())
}

/// 証明書ファイルの更新を検知して読み直す
fn spawn_reload(config: RustlsConfig, cert: PathBuf, key: PathBuf) {
    tokio::spawn(async move {
        let mut last = certificate_modified(&cert, &key).await;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let current = certificate_modified(&cert, &key).await;
            if current == last {
                continue;
            }
            // 読み直しでもセッション再開の設定を保つため、同じ手順で組み立て直す
            match load_server_config(&cert, &key).await {
                Ok(updated) => {
                    config.reload_from_config(updated);
                    println!("reloaded tls certificate from {}", cert.display());
                    last = current;
                }
                Err(error) => {
                    // 更新の途中で失敗しても、次の確認で読み直せるよう last は更新しない
                    eprintln!("failed to reload tls certificate: {error}");
                }
            }
        }
    });
}

/// 証明書と鍵の更新時刻を返す。ファイルがない場合は None
async fn certificate_modified(cert: &Path, key: &Path) -> Option<(SystemTime, SystemTime)> {
    let cert_time = tokio::fs::metadata(cert).await.ok()?.modified().ok()?;
    let key_time = tokio::fs::metadata(key).await.ok()?.modified().ok()?;
    Some((cert_time, key_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "edamame-worker-tls-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// ACME のキャッシュを所有者だけが読めるようにする
    #[tokio::test]
    async fn restricts_acme_cache_to_owner() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("permissions");
            restrict_to_owner(&dir).await.unwrap();
            let mode = tokio::fs::metadata(&dir)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// ハンドシェイクが終わらない接続をタイムアウトで閉じる
    #[tokio::test(start_paused = true)]
    async fn times_out_stalled_handshake() {
        use axum_server::accept::Accept;

        #[derive(Clone)]
        struct NeverAcceptor;

        impl<I, S> axum_server::accept::Accept<I, S> for NeverAcceptor
        where
            I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
            S: Send + 'static,
        {
            type Stream = I;
            type Service = S;
            type Future = std::future::Pending<std::io::Result<(I, S)>>;

            fn accept(&self, _stream: I, _service: S) -> Self::Future {
                std::future::pending()
            }
        }

        let acceptor = HandshakeTimeoutAcceptor {
            inner: NeverAcceptor,
            timeout: Duration::from_secs(10),
        };
        let (_client, server) = tokio::io::duplex(64);
        let result = acceptor.accept(server, ()).await;
        assert!(matches!(
            result,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    /// 配信中の証明書を client の検証で確かめる
    #[tokio::test]
    async fn probes_served_certificate() {
        let dir = temp_dir("probe");
        // テスト用の CA と、その CA が署名するサーバー証明書を作る
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "edamame test ca");
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, leaf_cert.pem()).unwrap();
        std::fs::write(&key, leaf_key.serialize_pem()).unwrap();

        let config = static_config(cert, key).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let app = Router::new();
        tokio::spawn(async move {
            serve_static(listener, app, config).await.unwrap();
        });

        // 自己署名の CA を root として信頼すれば検証できる。サーバーの起動を待ってから確かめる
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let trusted = client_config(roots);
        assert!(probe_certificate_with_retry("localhost", addr, &trusted).await);
        // テスト用の CA を信頼しない client では検証できない
        assert!(!probe_certificate("localhost", addr, &probe_client_config()).await);
        let _ = std::fs::remove_dir_all(dir);
    }
}
