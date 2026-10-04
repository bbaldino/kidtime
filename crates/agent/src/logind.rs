//! User session state from systemd-logind.

use protocol::UserState;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn get_user(&self, uid: u32) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.User",
    default_service = "org.freedesktop.login1"
)]
trait User {
    #[zbus(property)]
    fn sessions(&self) -> zbus::Result<Vec<(String, OwnedObjectPath)>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait Session {
    #[zbus(property, name = "Type")]
    fn session_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn idle_hint(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

const GRAPHICAL_TYPES: &[&str] = &["wayland", "x11", "mir"];

pub struct Logind {
    conn: zbus::Connection,
}

impl Logind {
    pub async fn connect() -> zbus::Result<Self> {
        Ok(Self {
            conn: zbus::Connection::system().await?,
        })
    }

    /// Current state of the user's graphical sessions.
    pub async fn user_state(&self, uid: u32) -> zbus::Result<UserState> {
        let manager = ManagerProxy::new(&self.conn).await?;
        // GetUser fails when the user has no sessions at all
        let Ok(user_path) = manager.get_user(uid).await else {
            return Ok(UserState::Offline);
        };
        let user = UserProxy::builder(&self.conn)
            .path(user_path)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;

        let mut state = UserState::Offline;
        for (_, path) in user.sessions().await? {
            let session = SessionProxy::builder(&self.conn)
                .path(path)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            // Sessions can disappear between listing and querying; skip those
            let Ok(kind) = session.session_type().await else {
                continue;
            };
            if !GRAPHICAL_TYPES.contains(&kind.as_str()) || session.class().await? != "user" {
                continue;
            }
            if session.state().await? != "active" {
                state = UserState::Background;
                continue;
            }
            // A foreground session decides the state outright
            return Ok(if session.locked_hint().await? {
                UserState::Locked
            } else if session.idle_hint().await? {
                UserState::Idle
            } else {
                UserState::Active
            });
        }
        Ok(state)
    }

    /// The user's graphical sessions on this PC, and whether each is locked.
    #[allow(dead_code)] // Task 7 wires this into the enforcement loop
    pub async fn graphical_sessions(
        &self,
        uid: u32,
    ) -> zbus::Result<Vec<crate::enforce::SessionInfo>> {
        let manager = ManagerProxy::new(&self.conn).await?;
        let Ok(user_path) = manager.get_user(uid).await else {
            return Ok(Vec::new());
        };
        let user = UserProxy::builder(&self.conn)
            .path(user_path)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let mut out = Vec::new();
        for (id, path) in user.sessions().await? {
            let session = SessionProxy::builder(&self.conn)
                .path(path)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            // Sessions can disappear between listing and querying; skip those
            let Ok(kind) = session.session_type().await else {
                continue;
            };
            if GRAPHICAL_TYPES.contains(&kind.as_str()) && session.class().await? == "user" {
                out.push(crate::enforce::SessionInfo {
                    id,
                    locked: session.locked_hint().await?,
                });
            }
        }
        Ok(out)
    }
}
