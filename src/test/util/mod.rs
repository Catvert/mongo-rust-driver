mod event;
mod failpoint;
mod lock;
mod matchable;

pub use self::{
    event::{CommandEvent, EventClient},
    failpoint::{FailCommandOptions, FailPoint, FailPointMode},
    lock::TestLock,
    matchable::{assert_matches, Matchable},
};

use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Duration};

use crate::bson::{doc, oid::ObjectId, Bson};
use semver::{Version, VersionReq};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use self::event::EventHandler;
use super::CLIENT_OPTIONS;
use crate::{
    error::{CommandError, ErrorKind, Result},
    operation::RunCommand,
    options::{AuthMechanism, ClientOptions, CollectionOptions, CreateCollectionOptions},
    Client,
    Collection,
};
use failpoint::FailPointGuard;

#[derive(Clone)]
pub struct TestClient {
    client: Client,
    pub options: ClientOptions,
    pub server_info: IsMasterCommandResponse,
    pub server_version: Version,
}

impl std::ops::Deref for TestClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl TestClient {
    pub async fn new() -> Self {
        Self::with_options(None).await
    }

    pub async fn with_options(options: Option<ClientOptions>) -> Self {
        Self::with_handler(None, options).await
    }

    async fn with_handler(
        event_handler: Option<EventHandler>,
        options: impl Into<Option<ClientOptions>>,
    ) -> Self {
        let mut options = options.into().unwrap_or_else(|| CLIENT_OPTIONS.clone());

        if let Some(event_handler) = event_handler {
            let handler = Arc::new(event_handler);
            options.command_event_handler = Some(handler.clone());
            options.cmap_event_handler = Some(handler);
        }

        let client = Client::with_options(options.clone()).unwrap();

        // To avoid populating the session pool with leftover implicit sessions, we check out a
        // session here and immediately mark it as dirty, then use it with any operations we need.
        let mut session = client
            .start_implicit_session_with_timeout(Duration::from_secs(60 * 60))
            .await;
        session.mark_dirty();

        let is_master = RunCommand::new("admin".into(), doc! { "isMaster":  1 }, None).unwrap();

        let server_info = bson::from_bson(Bson::Document(
            client
                .execute_operation_with_session(is_master, &mut session)
                .await
                .unwrap(),
        ))
        .unwrap();

        let build_info = RunCommand::new("test".into(), doc! { "buildInfo":  1 }, None).unwrap();

        let response = client
            .execute_operation_with_session(build_info, &mut session)
            .await
            .unwrap();

        let info: BuildInfo = bson::from_bson(Bson::Document(response)).unwrap();
        let server_version = info.version.split('-').next().unwrap();
        let server_version = Version::parse(server_version).unwrap();

        Self {
            client,
            options,
            server_info,
            server_version,
        }
    }

    pub async fn with_additional_options(
        options: Option<ClientOptions>,
        use_multiple_mongoses: bool,
    ) -> Self {
        let mut options = match options {
            Some(mut options) => {
                options.merge(CLIENT_OPTIONS.clone());
                options
            }
            None => CLIENT_OPTIONS.clone(),
        };
        if !use_multiple_mongoses && Self::new().await.is_sharded() {
            options.hosts = options.hosts.iter().cloned().take(1).collect();
        }
        Self::with_options(Some(options)).await
    }

    pub async fn create_user(
        &self,
        user: &str,
        pwd: impl Into<Option<&str>>,
        roles: &[Bson],
        mechanisms: &[AuthMechanism],
        db: impl Into<Option<&str>>,
    ) -> Result<()> {
        let mut cmd = doc! { "createUser": user, "roles": roles };

        if let Some(pwd) = pwd.into() {
            cmd.insert("pwd", pwd);
        }

        if self.server_version_gte(4, 0) && !mechanisms.is_empty() {
            let ms: bson::Array = mechanisms.iter().map(|s| Bson::from(s.as_str())).collect();
            cmd.insert("mechanisms", ms);
        }
        self.database(db.into().unwrap_or("admin"))
            .run_command(cmd, None)
            .await?;
        Ok(())
    }

    pub async fn drop_and_create_user(
        &self,
        user: &str,
        pwd: impl Into<Option<&str>>,
        roles: &[Bson],
        mechanisms: &[AuthMechanism],
        db: Option<&str>,
    ) -> Result<()> {
        let drop_user_result = self
            .database(db.unwrap_or("admin"))
            .run_command(doc! { "dropUser": user }, None)
            .await;

        match drop_user_result.as_ref().map_err(|e| e.as_ref()) {
            Err(ErrorKind::CommandError(CommandError { code: 11, .. })) | Ok(_) => {}
            e @ Err(_) => {
                e.unwrap();
            }
        };

        self.create_user(user, pwd, roles, mechanisms, db).await
    }

    pub fn get_coll(&self, db_name: &str, coll_name: &str) -> Collection {
        self.database(db_name).collection(coll_name)
    }

    pub async fn init_db_and_coll(&self, db_name: &str, coll_name: &str) -> Collection {
        let coll = self.get_coll(db_name, coll_name);
        drop_collection(&coll).await;
        coll
    }

    pub async fn init_db_and_typed_coll<T>(&self, db_name: &str, coll_name: &str) -> Collection<T>
    where
        T: Serialize + DeserializeOwned + Unpin + Debug,
    {
        let coll = self.database(db_name).collection_with_type(coll_name);
        drop_collection(&coll).await;
        coll
    }

    pub fn get_coll_with_options(
        &self,
        db_name: &str,
        coll_name: &str,
        options: CollectionOptions,
    ) -> Collection {
        self.database(db_name)
            .collection_with_options(coll_name, options)
    }

    pub async fn init_db_and_coll_with_options(
        &self,
        db_name: &str,
        coll_name: &str,
        options: CollectionOptions,
    ) -> Collection {
        let coll = self.get_coll_with_options(db_name, coll_name, options);
        drop_collection(&coll).await;
        coll
    }

    pub async fn create_fresh_collection(
        &self,
        db_name: &str,
        coll_name: &str,
        options: impl Into<Option<CreateCollectionOptions>>,
    ) -> Collection {
        self.drop_collection(db_name, coll_name).await;
        self.database(db_name)
            .create_collection(coll_name, options)
            .await
            .unwrap();

        self.get_coll(db_name, coll_name)
    }

    pub async fn supports_fail_command(&self) -> bool {
        let version = if self.is_sharded() {
            VersionReq::parse(">= 4.1.5").unwrap()
        } else {
            VersionReq::parse(">= 4.0").unwrap()
        };
        version.matches(&self.server_version)
    }

    pub async fn enable_failpoint(&self, fp: FailPoint) -> Result<FailPointGuard> {
        fp.enable(self).await
    }

    pub fn auth_enabled(&self) -> bool {
        self.options.credential.is_some()
    }

    pub fn is_standalone(&self) -> bool {
        !self.is_replica_set() && !self.is_sharded()
    }

    pub fn is_replica_set(&self) -> bool {
        self.options.repl_set_name.is_some()
    }

    pub fn is_sharded(&self) -> bool {
        self.server_info.msg.as_deref() == Some("isdbgrid")
    }

    #[allow(dead_code)]
    pub fn server_version_eq(&self, major: u64, minor: u64) -> bool {
        self.server_version.major == major && self.server_version.minor == minor
    }

    #[allow(dead_code)]
    pub fn server_version_gt(&self, major: u64, minor: u64) -> bool {
        self.server_version.major > major
            || (self.server_version.major == major && self.server_version.minor > minor)
    }

    pub fn server_version_gte(&self, major: u64, minor: u64) -> bool {
        self.server_version.major > major
            || (self.server_version.major == major && self.server_version.minor >= minor)
    }

    pub fn server_version_lt(&self, major: u64, minor: u64) -> bool {
        self.server_version.major < major
            || (self.server_version.major == major && self.server_version.minor < minor)
    }

    #[allow(dead_code)]
    pub fn server_version_lte(&self, major: u64, minor: u64) -> bool {
        self.server_version.major < major
            || (self.server_version.major == major && self.server_version.minor <= minor)
    }

    pub async fn drop_collection(&self, db_name: &str, coll_name: &str) {
        let coll = self.get_coll(db_name, coll_name);
        drop_collection(&coll).await;
    }
}

pub async fn drop_collection<T>(coll: &Collection<T>)
where
    T: Serialize + DeserializeOwned + Unpin + Debug,
{
    match coll.drop(None).await.as_ref().map_err(|e| e.as_ref()) {
        Err(ErrorKind::CommandError(CommandError { code: 26, .. })) | Ok(_) => {}
        e @ Err(_) => {
            e.unwrap();
        }
    };
}

#[derive(Debug, Deserialize)]
struct BuildInfo {
    version: String,
}

// Copy of the internal isMaster struct; fix this later.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IsMasterCommandResponse {
    #[serde(rename = "ismaster")]
    pub is_master: Option<bool>,
    pub ok: Option<f32>,
    pub hosts: Option<Vec<String>>,
    pub passives: Option<Vec<String>>,
    pub arbiters: Option<Vec<String>>,
    pub msg: Option<String>,
    pub me: Option<String>,
    pub set_version: Option<i32>,
    pub set_name: Option<String>,
    pub hidden: Option<bool>,
    pub secondary: Option<bool>,
    pub arbiter_only: Option<bool>,
    #[serde(rename = "isreplicaset")]
    pub is_replica_set: Option<bool>,
    pub logical_session_timeout_minutes: Option<i64>,
    pub min_wire_version: Option<i32>,
    pub max_wire_version: Option<i32>,
    pub tags: Option<HashMap<String, String>>,
    pub election_id: Option<ObjectId>,
    pub primary: Option<String>,
}

pub fn get_db_name(description: &str) -> String {
    let mut db_name = description.replace('$', "%").replace(' ', "_");
    // database names must have fewer than 64 characters
    db_name.truncate(63);
    db_name
}
