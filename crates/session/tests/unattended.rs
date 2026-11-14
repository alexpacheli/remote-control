//! The unattended password, end to end on real connections: nobody accepts,
//! so the password is all that lets a viewer in.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use hext_codec::{DecoderPreference, EncoderSettings};
use hext_crypto::pake::Verifier;
use hext_crypto::{DeviceKey, PublicKey};
use hext_net::{Endpoint, EndpointConfig, NetError, PasswordLogin};
use hext_proto::{Hello, HelloAsks, PeerInfo, Permissions, RejectReason};
use hext_session::host::{Attempt, HostConfig, LocalDesktop, UnattendedAccess, UnattendedPassword};
use hext_session::{
    Grant, KeptResume, Lockout, ResumeTokens, SessionCodes, SessionError, client, host,
};
use zeroize::Zeroizing;

const PASSWORD: &str = "cavalo correto";

/// The password as a service keeps it; `None`: the access is off.
struct Kept {
    password: Option<&'static str>,
    audit: Mutex<Vec<String>>,
    /// The technicians bound, as (key, ID).
    bound: Mutex<Vec<(PublicKey, String)>>,
    /// The accounts asked to bind to, as (key, chain).
    accounts: Mutex<Vec<(PublicKey, [u8; 32])>>,
}

impl UnattendedPassword for Kept {
    fn verifier(&self) -> Option<Verifier> {
        self.password
            .map(|password| Verifier::new(password).unwrap())
    }

    fn audit(&self, entry: &str) {
        self.audit.lock().unwrap().push(entry.to_owned());
    }

    fn bind(&self, viewer: &PublicKey, grant: &Grant) -> bool {
        self.bound.lock().unwrap().push((*viewer, grant.id.clone()));
        true
    }

    fn bind_account(&self, viewer: &PublicKey, chain: [u8; 32], _grant: &Grant) {
        self.accounts.lock().unwrap().push((*viewer, chain));
    }
}

/// Stands in for restarting the computer.
#[derive(Default)]
struct Restarts {
    asked: Mutex<Vec<String>>,
    revoked: Mutex<bool>,
}

impl host::Restart for Restarts {
    fn restart(&self, kept: &KeptResume, who: &str) -> Result<(), String> {
        assert_eq!(kept.grant.id, "482 913 557");
        self.asked.lock().unwrap().push(who.to_owned());
        Ok(())
    }

    fn revoke(&self) {
        *self.revoked.lock().unwrap() = true;
    }
}

/// The indicator's Encerrar.
#[derive(Default)]
struct Bar(Arc<Notify>);

impl host::Presence for Bar {
    fn started(&self, _: &host::ActiveSession) -> Arc<Notify> {
        self.0.clone()
    }

    fn ended(&self) {}
}

struct Host {
    endpoint: Endpoint,
    codes: SessionCodes,
    lockout: Lockout,
    resumes: ResumeTokens,
    /// What would go to the server.
    outcomes: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Attempt>>,
    kept: Arc<Kept>,
    restarts: Arc<Restarts>,
    encerrar: Arc<Notify>,
    /// The viewers' endpoints: a session dies with its own.
    viewers: Mutex<Vec<Endpoint>>,
}

impl Host {
    /// A host that serves sessions until the test ends.
    async fn start(password: Option<&'static str>) -> Self {
        let key = DeviceKey::generate().unwrap();
        let accept = EndpointConfig {
            accept: true,
            ..Default::default()
        };
        let endpoint = hext_net::bind(&key, &accept).await.unwrap();
        let (codes, lockout) = (SessionCodes::default(), Lockout::default());
        let resumes = ResumeTokens::default();
        let restarts = Arc::new(Restarts::default());
        let encerrar = Arc::new(Notify::new());
        let kept = Arc::new(Kept {
            password,
            audit: Mutex::default(),
            bound: Mutex::default(),
            accounts: Mutex::default(),
        });
        let (attempts, outcomes) = tokio::sync::mpsc::unbounded_channel();
        let config = Arc::new(HostConfig {
            owners: None,
            peer: PeerInfo::local("test", "host"),
            access_code: None,
            session_codes: codes.clone(),
            desktop: Arc::new(LocalDesktop {
                virtual_display: false,
                display: 0,
                test_pattern: true,
                encoder: EncoderSettings::default(),
            }),
            presence: Some(Arc::new(Bar(encerrar.clone()))),
            unattended: Some(UnattendedAccess {
                key: key.public_key(),
                lockout: lockout.clone(),
                password: kept.clone(),
                attempts,
            }),
            resumes: resumes.clone(),
            restart: Some(restarts.clone()),
            files: None,
        });
        let accepting = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        let _ = host::serve(conn, config).await;
                    }
                });
            }
        });
        Self {
            endpoint,
            codes,
            lockout,
            resumes,
            outcomes: tokio::sync::Mutex::new(outcomes),
            kept,
            restarts,
            encerrar,
            viewers: Mutex::default(),
        }
    }

    /// Tries to open a session as `viewer`, which got a code from the server
    /// without anyone accepting.
    async fn attempt(
        &self,
        viewer: &DeviceKey,
        password: Option<&str>,
    ) -> hext_session::Result<client::ClientHandle> {
        self.open(viewer, true, password, None).await
    }

    /// Opens a session as `viewer`, which someone accepted.
    async fn attempt_accepted(
        &self,
        viewer: &DeviceKey,
    ) -> hext_session::Result<client::ClientHandle> {
        self.open(viewer, false, None, None).await
    }

    /// The same, with `permissions`.
    async fn attempt_with(
        &self,
        viewer: &DeviceKey,
        permissions: Permissions,
    ) -> hext_session::Result<client::ClientHandle> {
        self.issue_and_connect(viewer, permissions, false, None, None)
            .await
    }

    /// Tries to come back as `viewer` with `token`, nobody accepting.
    async fn attempt_back(
        &self,
        viewer: &DeviceKey,
        token: [u8; 32],
    ) -> hext_session::Result<client::ClientHandle> {
        self.open(viewer, true, None, Some(token)).await
    }

    async fn open(
        &self,
        viewer: &DeviceKey,
        needs_password: bool,
        password: Option<&str>,
        resume: Option<[u8; 32]>,
    ) -> hext_session::Result<client::ClientHandle> {
        self.issue_and_connect(
            viewer,
            Permissions::CONTROL,
            needs_password,
            password,
            resume,
        )
        .await
    }

    async fn issue_and_connect(
        &self,
        viewer: &DeviceKey,
        permissions: Permissions,
        needs_password: bool,
        password: Option<&str>,
        resume: Option<[u8; 32]>,
    ) -> hext_session::Result<client::ClientHandle> {
        let grant = Grant {
            permissions,
            name: "Técnico".into(),
            id: "482 913 557".into(),
            needs_password,
            remember: false,
            bound: false,
            via: None,
            account: None,
        };
        self.connect_with(viewer, grant, password, resume).await
    }

    /// Opens a session as `viewer`, which the server sent with `grant`, the
    /// viewer asking what the server passed on.
    async fn connect_with(
        &self,
        viewer: &DeviceKey,
        grant: Grant,
        password: Option<&str>,
        resume: Option<[u8; 32]>,
    ) -> hext_session::Result<client::ClientHandle> {
        let asks = HelloAsks {
            remember: grant.remember,
            account: grant.account,
        };
        self.connect_asking(viewer, grant, asks, password, resume)
            .await
    }

    /// Opens a session as `viewer`, which the server sent with `grant`, the
    /// viewer itself asking `asks`.
    async fn connect_asking(
        &self,
        viewer: &DeviceKey,
        grant: Grant,
        asks: HelloAsks,
        password: Option<&str>,
        resume: Option<[u8; 32]>,
    ) -> hext_session::Result<client::ClientHandle> {
        let code = self.codes.issue(viewer.public_key(), grant);
        let endpoint = hext_net::bind(viewer, &EndpointConfig::default())
            .await
            .unwrap();
        self.viewers.lock().unwrap().push(endpoint.clone());
        let conn = hext_net::connect(&endpoint, self.endpoint.addr())
            .await
            .unwrap();
        let login = password.map(|password| PasswordLogin {
            password: Zeroizing::new(password.to_owned()),
            viewer: viewer.public_key(),
        });
        client::start(
            conn,
            Hello {
                peer: PeerInfo::local("test", "viewer"),
                access_code: code,
                resume,
                asks,
                audio: false,
            },
            login,
            DecoderPreference::Software,
            |_| {},
        )
        .await
    }

    /// The next outcome reported for the server.
    async fn outcome(&self) -> Attempt {
        let mut outcomes = self.outcomes.lock().await;
        tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
            .await
            .expect("an outcome in time")
            .expect("the host is on")
    }

    /// The audit, once it has `lines` lines: the host finishes up after the
    /// viewer has gone.
    async fn audit(&self, lines: usize) -> Vec<String> {
        for _ in 0..500 {
            let audit = self.kept.audit.lock().unwrap().clone();
            if audit.len() >= lines {
                return audit;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the audit never got {lines} lines");
    }
}

fn rejected(result: hext_session::Result<client::ClientHandle>) -> Option<RejectReason> {
    match result {
        Err(SessionError::Net(NetError::Rejected(reason))) => Some(reason),
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_right_password_opens_a_session_and_it_is_audited() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let session = host.attempt(&viewer, Some(PASSWORD)).await.unwrap();
    assert_eq!(session.permissions, Permissions::CONTROL);
    session.close().await.unwrap();

    let audit = host.audit(2).await;
    assert!(audit[0].starts_with("sessão iniciada"), "{audit:?}");
    assert!(audit[0].contains("482 913 557"), "{audit:?}");
    assert!(audit[1].starts_with("sessão encerrada"), "{audit:?}");
}

/// What the server sends for a technician asking with the password, and to
/// be remembered.
fn remembering() -> Grant {
    Grant {
        permissions: Permissions::CONTROL,
        name: "Técnico".into(),
        id: "482 913 557".into(),
        needs_password: true,
        remember: true,
        bound: false,
        via: None,
        account: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_to_be_remembered_binds_the_technician_after_the_password() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let session = host
        .connect_with(&viewer, remembering(), Some(PASSWORD), None)
        .await
        .unwrap();
    session.close().await.unwrap();
    assert_eq!(
        *host.kept.bound.lock().unwrap(),
        [(viewer.public_key(), "482 913 557".to_owned())]
    );
    let audit = host.audit(3).await;
    assert!(audit[0].starts_with("técnico vinculado"), "{audit:?}");
    assert!(audit[1].starts_with("sessão iniciada"), "{audit:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_binds_nobody() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let wrong = host
        .connect_with(&viewer, remembering(), Some("cavalo errado"), None)
        .await;
    assert_eq!(rejected(wrong), Some(RejectReason::WrongPassword));
    assert!(host.kept.bound.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_to_bind_to_an_account_takes_the_password() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let to_account = Grant {
        remember: false,
        account: Some([9; 32]),
        ..remembering()
    };
    let wrong = host
        .connect_with(&viewer, to_account.clone(), Some("cavalo errado"), None)
        .await;
    assert_eq!(rejected(wrong), Some(RejectReason::WrongPassword));
    assert!(host.kept.accounts.lock().unwrap().is_empty());
    let session = host
        .connect_with(&viewer, to_account, Some(PASSWORD), None)
        .await
        .unwrap();
    session.close().await.unwrap();
    assert_eq!(
        *host.kept.accounts.lock().unwrap(),
        [(viewer.public_key(), [9; 32])]
    );
    // Not as a technician of this computer: by the account, once on its list.
    assert!(host.kept.bound.lock().unwrap().is_empty());
}

/// The server says the technician asked, but the technician itself did not
/// (or is from before it could say so): the server's word is not enough.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_server_asking_binds_nobody() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let grant = Grant {
        account: Some([9; 32]),
        ..remembering()
    };
    let session = host
        .connect_asking(
            &viewer,
            grant.clone(),
            HelloAsks::default(),
            Some(PASSWORD),
            None,
        )
        .await
        .unwrap();
    session.close().await.unwrap();
    // Asking for another account than the one signed binds nothing either.
    let other = HelloAsks {
        remember: false,
        account: Some([8; 32]),
    };
    let session = host
        .connect_asking(&viewer, grant, other, Some(PASSWORD), None)
        .await
        .unwrap();
    session.close().await.unwrap();
    host.audit(4).await;
    assert!(host.kept.bound.lock().unwrap().is_empty());
    assert!(host.kept.accounts.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_asking_nobody_is_bound() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let session = host.attempt(&viewer, Some(PASSWORD)).await.unwrap();
    session.close().await.unwrap();
    host.audit(2).await;
    assert!(host.kept.bound.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_technician_gets_in_without_the_password_and_it_is_audited() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let grant = Grant {
        needs_password: false,
        remember: false,
        bound: true,
        via: None,
        account: None,
        ..remembering()
    };
    let session = host.connect_with(&viewer, grant, None, None).await.unwrap();
    session.close().await.unwrap();
    let audit = host.audit(2).await;
    assert!(
        audit[0].starts_with("sessão iniciada (técnico vinculado"),
        "{audit:?}"
    );
    assert!(audit[1].starts_with("sessão encerrada"), "{audit:?}");
    // Its code is good from its own device only.
    let other = DeviceKey::generate().unwrap();
    let code = host.codes.issue(
        viewer.public_key(),
        Grant {
            needs_password: false,
            remember: false,
            bound: true,
            via: None,
            account: None,
            ..remembering()
        },
    );
    assert!(host.codes.redeem(&code, &other.public_key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_stays_out_and_counts() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    assert_eq!(
        rejected(host.attempt(&viewer, Some("cavalo errado")).await),
        Some(RejectReason::WrongPassword)
    );
    assert!(host.audit(1).await[0].contains("WrongPassword"));

    for _ in 0..4 {
        host.lockout.failed(&viewer.public_key());
    }
    // The wait now holds, and it comes before any exchange.
    let Some(RejectReason::LockedOut { seconds }) =
        rejected(host.attempt(&viewer, Some(PASSWORD)).await)
    else {
        panic!("expected to wait");
    };
    assert!(seconds > 0 && seconds <= 60, "{seconds}");
    // Another technician is not held up by it.
    let other = DeviceKey::generate().unwrap();
    host.attempt(&other, Some(PASSWORD)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_hears_how_each_attempt_went() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let went = |entered| Attempt {
        viewer: "482 913 557".into(),
        entered,
    };

    assert_eq!(
        rejected(host.attempt(&viewer, Some("cavalo errado")).await),
        Some(RejectReason::WrongPassword)
    );
    assert_eq!(host.outcome().await, went(false));

    let first = host.attempt(&viewer, Some(PASSWORD)).await.unwrap();
    assert_eq!(host.outcome().await, went(true));

    // Coming back with the token counts as much as the password.
    let token = token_of(&first).await;
    assert!(host.resumes.displace(&viewer.public_key()));
    first.closed().await;
    host.attempt_back(&viewer, token).await.unwrap();
    assert_eq!(host.outcome().await, went(true));

    // Held back or turned away, nothing was tried: nothing to report.
    host.resumes.displace(&viewer.public_key());
    for _ in 0..5 {
        host.lockout.failed(&viewer.public_key());
    }
    assert!(matches!(
        rejected(host.attempt(&viewer, Some(PASSWORD)).await),
        Some(RejectReason::LockedOut { .. })
    ));
    assert!(host.outcomes.lock().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_password_the_viewer_stays_out() {
    let host = Host::start(Some(PASSWORD)).await;
    let viewer = DeviceKey::generate().unwrap();
    let result = host.attempt(&viewer, None).await;
    assert!(matches!(
        result,
        Err(SessionError::Net(NetError::PasswordNeeded))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_access_off_no_password_gets_in() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    assert_eq!(
        rejected(host.attempt(&viewer, Some(PASSWORD)).await),
        Some(RejectReason::Denied)
    );
}

/// The host's answer about restarting, once it came.
async fn restart_answer(session: &client::ClientHandle) -> bool {
    for _ in 0..500 {
        if let Some(answer) = session.restart_answer() {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the host never answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_permission_the_computer_does_not_restart() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    let session = host.attempt_accepted(&viewer).await.unwrap();
    token_of(&session).await;
    session.request_restart();
    assert!(!restart_answer(&session).await);
    assert!(host.restarts.asked.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_permission_it_restarts_once() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    let permissions = Permissions {
        restart: true,
        ..Permissions::CONTROL
    };
    let session = host.attempt_with(&viewer, permissions).await.unwrap();
    assert!(session.permissions.restart);
    token_of(&session).await;
    session.request_restart();
    assert!(restart_answer(&session).await);
    session.request_restart();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(session.restart_answer(), Some(true));
    assert_eq!(host.restarts.asked.lock().unwrap().len(), 1);
    assert!(!*host.restarts.revoked.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn encerrar_during_a_restart_is_the_end() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    let permissions = Permissions {
        restart: true,
        ..Permissions::CONTROL
    };
    let session = host.attempt_with(&viewer, permissions).await.unwrap();
    token_of(&session).await;
    session.request_restart();
    assert!(restart_answer(&session).await);

    host.encerrar.notify_one();
    session.closed().await;
    assert!(*host.restarts.revoked.lock().unwrap());
    assert_eq!(session.comes_back(), None);
}

/// The token the host gave `session`, once it arrived.
async fn token_of(session: &client::ClientHandle) -> [u8; 32] {
    for _ in 0..500 {
        if let Some(token) = session.token() {
            return token;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the host never gave a token");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_comes_back_with_its_token_without_the_password() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    // As if accepted by someone: the session has a token.
    let first = host.attempt_accepted(&viewer).await.unwrap();
    let token = token_of(&first).await;

    // The connection is gone; the host has not noticed.
    assert!(host.resumes.displace(&viewer.public_key()));
    first.closed().await;

    let back = host.attempt_back(&viewer, token).await.unwrap();
    assert_eq!(back.permissions, Permissions::CONTROL);
    // (Accepted at the computer: audited from its start, then its end.)
    let audit = host.audit(3).await;
    assert!(
        audit[0].starts_with("sessão iniciada (aceita no computador)"),
        "{audit:?}"
    );
    assert!(
        audit.iter().any(|line| line.starts_with("sessão retomada")),
        "{audit:?}"
    );

    // Once only: the new session has its own.
    assert_ne!(token_of(&back).await, token);
    assert!(matches!(
        host.attempt_back(&viewer, token).await,
        Err(SessionError::Net(NetError::Rejected(RejectReason::Denied)))
    ));

    // What came back is audited to its end, whoever let it in.
    back.close().await.unwrap();
    let audit = host.audit(4).await;
    assert!(
        audit
            .iter()
            .any(|line| line.starts_with("sessão encerrada")),
        "{audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_is_no_good_to_another_viewer() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    let first = host.attempt_accepted(&viewer).await.unwrap();
    let token = token_of(&first).await;
    assert!(host.resumes.displace(&viewer.public_key()));
    first.closed().await;

    let thief = DeviceKey::generate().unwrap();
    assert!(matches!(
        host.attempt_back(&thief, token).await,
        Err(SessionError::Net(NetError::Rejected(RejectReason::Denied)))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_ended_properly_does_not_come_back() {
    let host = Host::start(None).await;
    let viewer = DeviceKey::generate().unwrap();
    let first = host.attempt_accepted(&viewer).await.unwrap();
    let token = token_of(&first).await;
    first.close().await.unwrap();
    for _ in 0..500 {
        if !host.resumes.displace(&viewer.public_key()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!host.resumes.displace(&viewer.public_key()));
    assert!(matches!(
        host.attempt_back(&viewer, token).await,
        Err(SessionError::Net(NetError::Rejected(RejectReason::Denied)))
    ));
}

#[test]
fn a_viewer_cut_off_from_the_host_comes_back() {
    let hosting = tokio::runtime::Runtime::new().unwrap();
    let viewing = tokio::runtime::Runtime::new().unwrap();
    let host = hosting.block_on(Host::start(None));
    let viewer = DeviceKey::generate().unwrap();
    viewing.block_on(async {
        let session = host.attempt_accepted(&viewer).await.unwrap();
        let token = token_of(&session).await;

        // Gone without a word, as when the network drops.
        hosting.shutdown_background();
        tokio::time::timeout(Duration::from_secs(30), session.closed())
            .await
            .expect("the idle timeout ends the connection");
        // The session winds down after the connection does.
        for _ in 0..500 {
            if session.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (back, _) = session.comes_back().expect("a lost connection comes back");
        assert_eq!(back, token);
    });
}
