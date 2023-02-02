use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
};

use anyhow::Result;
use clippy_utilities::Cast;
use curp::{cmd::ProposeId, error::ExecuteError};
use itertools::Itertools;
use jsonwebtoken::{DecodingKey, EncodingKey};
use log::debug;
use parking_lot::{Mutex, RwLock};
use pbkdf2::{
    password_hash::{PasswordHash, PasswordVerifier},
    Pbkdf2,
};
use prost::Message;
use tokio::sync::mpsc;
use utils::parking_lot_lock::RwLockMap;

use super::perms::{JwtTokenManager, PermissionCache, TokenClaims, TokenOperate, UserPermissions};
use crate::{
    header_gen::HeaderGenerator,
    revision_number::RevisionNumber,
    rpc::{
        AuthDisableRequest, AuthDisableResponse, AuthEnableRequest, AuthEnableResponse,
        AuthRoleAddRequest, AuthRoleAddResponse, AuthRoleDeleteRequest, AuthRoleDeleteResponse,
        AuthRoleGetRequest, AuthRoleGetResponse, AuthRoleGrantPermissionRequest,
        AuthRoleGrantPermissionResponse, AuthRoleListRequest, AuthRoleListResponse,
        AuthRoleRevokePermissionRequest, AuthRoleRevokePermissionResponse, AuthStatusRequest,
        AuthStatusResponse, AuthUserAddRequest, AuthUserAddResponse, AuthUserChangePasswordRequest,
        AuthUserChangePasswordResponse, AuthUserDeleteRequest, AuthUserDeleteResponse,
        AuthUserGetRequest, AuthUserGetResponse, AuthUserGrantRoleRequest,
        AuthUserGrantRoleResponse, AuthUserListRequest, AuthUserListResponse,
        AuthUserRevokeRoleRequest, AuthUserRevokeRoleResponse, AuthenticateRequest,
        AuthenticateResponse, KeyValue, Permission, RequestWrapper, ResponseWrapper, Role, Type,
        User,
    },
    server::command::KeyRange,
    storage::{
        db::DB,
        index::{Index, IndexOperate},
        lease_store::{Lease, LeaseMessage},
        req_ctx::RequestCtx,
        storage_api::StorageApi,
    },
};

/// Key prefix of user
pub(crate) const USER_PREFIX: &[u8] = b"user/";
/// Key prefix of role
pub(crate) const ROLE_PREFIX: &[u8] = b"role/";
/// Key of `AuthEnable`
pub(crate) const AUTH_ENABLE_KEY: &[u8] = b"auth_enable";
/// Root user
pub(crate) const ROOT_USER: &str = "root";
/// Root role
pub(crate) const ROOT_ROLE: &str = "root";

/// Auth store inner
pub(crate) struct AuthStoreBackend<S>
where
    S: StorageApi,
{
    /// Key Index
    index: Index,
    /// DB to store key value
    db: DB<S>,
    /// Revision
    revision: RevisionNumber,
    /// Speculative execution pool. Mapping from propose id to request
    sp_exec_pool: Mutex<HashMap<ProposeId, RequestCtx>>,
    /// Enabled
    enabled: AtomicBool,
    /// Permission cache
    permission_cache: RwLock<PermissionCache>,
    /// The manager of token
    token_manager: Option<JwtTokenManager>,
    /// Lease command sender
    lease_cmd_tx: mpsc::Sender<LeaseMessage>,
    /// Header generator
    header_gen: Arc<HeaderGenerator>,
}

impl<S> fmt::Debug for AuthStoreBackend<S>
where
    S: StorageApi,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthStoreBackend")
            .field("index", &self.index)
            .field("db", &self.db)
            .field("revision", &self.revision)
            .field("sp_exec_pool", &self.sp_exec_pool)
            .field("enabled", &self.enabled)
            .field("permission_cache", &self.permission_cache)
            .field("lease_cmd_tx", &self.lease_cmd_tx)
            .field("header_gen", &self.header_gen)
            .finish()
    }
}

impl<S> AuthStoreBackend<S>
where
    S: StorageApi,
{
    /// New `AuthStoreBackend`
    pub(super) fn new(
        lease_cmd_tx: mpsc::Sender<LeaseMessage>,
        key_pair: Option<(EncodingKey, DecodingKey)>,
        header_gen: Arc<HeaderGenerator>,
        storage: S,
    ) -> Self {
        Self {
            index: Index::new(),
            db: DB::new(storage),
            revision: RevisionNumber::new(),
            sp_exec_pool: Mutex::new(HashMap::new()),
            enabled: AtomicBool::new(false),
            token_manager: key_pair.map(|(encoding_key, decoding_key)| {
                JwtTokenManager::new(encoding_key, decoding_key)
            }),
            permission_cache: RwLock::new(PermissionCache::new()),
            lease_cmd_tx,
            header_gen,
        }
    }

    /// Get Lease by lease id
    pub(super) async fn get_lease(&self, lease_id: i64) -> Option<Lease> {
        let (detach, rx) = LeaseMessage::look_up(lease_id);
        assert!(
            self.lease_cmd_tx.send(detach).await.is_ok(),
            "lease_cmd_tx is closed"
        );
        rx.await.unwrap_or_else(|_e| panic!("res sender is closed"))
    }

    /// Get revision of Auth store
    pub(crate) fn revision(&self) -> i64 {
        self.revision.get()
    }

    /// Get enabled of Auth store
    pub(super) fn is_enabled(&self) -> bool {
        self.enabled.load(AtomicOrdering::Acquire)
    }

    /// Check password
    pub(super) fn check_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<i64, ExecuteError> {
        if !self.is_enabled() {
            return Err(ExecuteError::InvalidCommand(
                "auth is not enabled".to_owned(),
            ));
        }
        let user = self.get_user(username)?;
        let need_password = user.options.as_ref().map_or(true, |o| !o.no_password);
        if !need_password {
            return Err(ExecuteError::InvalidCommand(
                "password was given for no password user".to_owned(),
            ));
        }

        let hash = String::from_utf8_lossy(&user.password);
        let hash = PasswordHash::new(&hash)
            .unwrap_or_else(|e| panic!("Failed to parse password hash, error: {e}"));
        Pbkdf2
            .verify_password(password.as_bytes(), &hash)
            .map_err(|e| ExecuteError::InvalidCommand(format!("verify password error: {e}")))?;

        Ok(self.revision())
    }

    /// Assign token
    pub(super) fn assign(&self, username: &str) -> Result<String, ExecuteError> {
        match self.token_manager {
            Some(ref token_manager) => token_manager
                .assign(username, self.revision())
                .map_err(|e| ExecuteError::InvalidCommand(format!("assign token error: {e}"))),
            None => Err(ExecuteError::InvalidCommand(
                "token_manager is not initialized".to_owned(),
            )),
        }
    }

    /// verify token
    pub(super) fn verify_token(&self, token: &str) -> Result<TokenClaims, ExecuteError> {
        match self.token_manager {
            Some(ref token_manager) => token_manager
                .verify(token)
                .map_err(|e| ExecuteError::InvalidCommand(format!("verify token error: {e}"))),
            None => Err(ExecuteError::InvalidCommand(
                "token_manager is not initialized".to_owned(),
            )),
        }
    }

    /// create permission cache
    fn create_permission_cache(&self) -> Result<(), ExecuteError> {
        let mut permission_cache = PermissionCache::new();
        for user in self.get_all_users()? {
            let user_permission = self.get_user_permissions(&user);
            let username = String::from_utf8_lossy(&user.name).to_string();
            let _ignore = permission_cache
                .user_permissions
                .insert(username, user_permission);
        }
        self.permission_cache
            .map_write(|mut cache| *cache = permission_cache);
        Ok(())
    }

    /// get user permissions
    fn get_user_permissions(&self, user: &User) -> UserPermissions {
        let mut user_permission = UserPermissions::new();
        for role_name in &user.roles {
            let role = match self.get_role(role_name) {
                Ok(role) => role,
                Err(_) => continue,
            };
            for permission in role.key_permission {
                let key_range = KeyRange {
                    start: permission.key,
                    end: permission.range_end,
                };
                #[allow(clippy::unwrap_used)] // safe unwrap
                match Type::from_i32(permission.perm_type).unwrap() {
                    Type::Readwrite => {
                        user_permission.read.push(key_range.clone());
                        user_permission.write.push(key_range.clone());
                    }
                    Type::Write => {
                        user_permission.write.push(key_range.clone());
                    }
                    Type::Read => {
                        user_permission.read.push(key_range.clone());
                    }
                }
            }
        }
        user_permission
    }

    /// get user permissions from cache
    pub(super) fn get_user_permissions_from_cache(
        &self,
        username: &str,
    ) -> Result<UserPermissions, ExecuteError> {
        self.permission_cache
            .map_read(|cache| match cache.user_permissions.get(username) {
                Some(user_permissions) => Ok(user_permissions.clone()),
                None => Err(ExecuteError::InvalidCommand(
                    "user permissions not found".to_owned(),
                )),
            })
    }

    /// get `KeyValue` in `AuthStore`
    fn get(&self, key: &[u8]) -> Result<Option<KeyValue>, ExecuteError> {
        let revisions = self.index.get(key, &[], 0);
        assert!(revisions.len() <= 1);
        self.db.get_values(&revisions).map(|mut kv| kv.pop())
    }

    /// get user by username
    pub(super) fn get_user(&self, username: &str) -> Result<User, ExecuteError> {
        let key = [USER_PREFIX, username.as_bytes()].concat();
        match self.get(&key)? {
            Some(kv) => Ok(User::decode(kv.value.as_slice()).unwrap_or_else(|e| {
                panic!(
                    "Failed to decode user from kv value, error: {:?}, kv: {:?}",
                    e, kv
                )
            })),
            None => Err(ExecuteError::InvalidCommand("user not found".to_owned())),
        }
    }

    /// get role by rolename
    fn get_role(&self, rolename: &str) -> Result<Role, ExecuteError> {
        let key = [ROLE_PREFIX, rolename.as_bytes()].concat();
        match self.get(&key)? {
            Some(kv) => Ok(Role::decode(kv.value.as_slice()).unwrap_or_else(|e| {
                panic!(
                    "Failed to decode role from kv value, error: {:?}, kv: {:?}",
                    e, kv
                )
            })),
            None => Err(ExecuteError::InvalidCommand("role not found".to_owned())),
        }
    }

    /// get `KeyValue` to `AuthStore`
    fn put(
        &self,
        key: Vec<u8>,
        value: Vec<u8>,
        revision: i64,
        sub_revision: i64,
    ) -> Result<(), ExecuteError> {
        let new_rev = self
            .index
            .insert_or_update_revision(&key, revision, sub_revision);
        let kv = KeyValue {
            key,
            value,
            create_revision: new_rev.create_revision,
            mod_revision: new_rev.mod_revision,
            version: new_rev.version,
            ..KeyValue::default()
        };
        self.db.insert(new_rev.as_revision(), &kv)
    }

    /// put user to `AuthStore`
    fn put_user(&self, user: &User, revision: i64, sub_revision: i64) -> Result<(), ExecuteError> {
        let key = [USER_PREFIX, &user.name].concat();
        let value = user.encode_to_vec();
        self.put(key, value, revision, sub_revision)
    }

    /// put role to `AuthStore`
    fn put_role(&self, role: &Role, revision: i64, sub_revision: i64) -> Result<(), ExecuteError> {
        let key = [ROLE_PREFIX, &role.name].concat();
        let value = role.encode_to_vec();
        self.put(key, value, revision, sub_revision)
    }

    /// delete `KeyValue` in `AuthStore`
    fn delete(&self, key: &[u8], revision: i64, sub_revision: i64) -> Result<(), ExecuteError> {
        let revisions = self.index.delete(key, &[], revision, sub_revision);
        let _ignore = self.db.mark_deletions(&revisions)?;
        Ok(())
    }

    /// delete user in `AuthStore`
    fn delete_user(
        &self,
        username: &str,
        revision: i64,
        sub_revision: i64,
    ) -> Result<(), ExecuteError> {
        let key = [USER_PREFIX, username.as_bytes()].concat();
        self.delete(&key, revision, sub_revision)
    }

    /// delete role in `AuthStore`
    fn delete_role(
        &self,
        rolename: &str,
        revision: i64,
        sub_revision: i64,
    ) -> Result<(), ExecuteError> {
        let key = [ROLE_PREFIX, rolename.as_bytes()].concat();
        self.delete(&key, revision, sub_revision)
    }

    /// Get all users in the `AuthStore`
    fn get_all_users(&self) -> Result<Vec<User>, ExecuteError> {
        let range_end = KeyRange::get_prefix(USER_PREFIX);
        let revisions = self.index.get(USER_PREFIX, &range_end, 0);
        let users = self
            .db
            .get_values(&revisions)?
            .into_iter()
            .map(|kv| {
                User::decode(kv.value.as_slice()).unwrap_or_else(|e| {
                    panic!(
                        "Failed to decode user from kv value, error: {:?}, kv: {:?}",
                        e, kv
                    )
                })
            })
            .collect();
        Ok(users)
    }

    /// Get all roles in the `AuthStore`
    fn get_all_roles(&self) -> Result<Vec<Role>, ExecuteError> {
        let range_end = KeyRange::get_prefix(ROLE_PREFIX);
        let revisions = self.index.get(ROLE_PREFIX, &range_end, 0);
        let roles = self
            .db
            .get_values(&revisions)?
            .into_iter()
            .map(|kv| {
                Role::decode(kv.value.as_slice()).unwrap_or_else(|e| {
                    panic!(
                        "Failed to decode role from kv value, error: {:?}, kv: {:?}",
                        e, kv
                    )
                })
            })
            .collect();
        Ok(roles)
    }

    /// Handle `InternalRequest`
    pub(super) fn handle_auth_req(
        &self,
        id: ProposeId,
        wrapper: RequestWrapper,
    ) -> Result<ResponseWrapper, ExecuteError> {
        // routed when call execute, other request will be routed to other backend
        #[allow(clippy::wildcard_enum_match_arm)]
        let res = match wrapper {
            RequestWrapper::AuthEnableRequest(ref req) => {
                self.handle_auth_enable_request(req).map(Into::into)
            }
            RequestWrapper::AuthDisableRequest(ref req) => {
                Ok(self.handle_auth_disable_request(req).into())
            }
            RequestWrapper::AuthStatusRequest(ref req) => {
                Ok(self.handle_auth_status_request(req).into())
            }
            RequestWrapper::AuthUserAddRequest(ref req) => {
                self.handle_user_add_request(req).map(Into::into)
            }
            RequestWrapper::AuthUserGetRequest(ref req) => {
                self.handle_user_get_request(req).map(Into::into)
            }
            RequestWrapper::AuthUserListRequest(ref req) => {
                self.handle_user_list_request(req).map(Into::into)
            }
            RequestWrapper::AuthUserGrantRoleRequest(ref req) => {
                self.handle_user_grant_role_request(req).map(Into::into)
            }
            RequestWrapper::AuthUserRevokeRoleRequest(ref req) => {
                self.handle_user_revoke_role_request(req).map(Into::into)
            }
            RequestWrapper::AuthUserChangePasswordRequest(ref req) => self
                .handle_user_change_password_request(req)
                .map(Into::into),
            RequestWrapper::AuthUserDeleteRequest(ref req) => {
                self.handle_user_delete_request(req).map(Into::into)
            }
            RequestWrapper::AuthRoleAddRequest(ref req) => {
                self.handle_role_add_request(req).map(Into::into)
            }
            RequestWrapper::AuthRoleGetRequest(ref req) => {
                self.handle_role_get_request(req).map(Into::into)
            }
            RequestWrapper::AuthRoleGrantPermissionRequest(ref req) => self
                .handle_role_grant_permission_request(req)
                .map(Into::into),
            RequestWrapper::AuthRoleRevokePermissionRequest(ref req) => self
                .handle_role_revoke_permission_request(req)
                .map(Into::into),
            RequestWrapper::AuthRoleDeleteRequest(ref req) => {
                self.handle_role_delete_request(req).map(Into::into)
            }
            RequestWrapper::AuthRoleListRequest(ref req) => {
                self.handle_role_list_request(req).map(Into::into)
            }
            RequestWrapper::AuthenticateRequest(ref req) => {
                self.handle_authenticate_request(req).map(Into::into)
            }
            _ => {
                unreachable!("Other request should not be sent to this store");
            }
        };
        let ctx = RequestCtx::new(wrapper, res.is_err());
        let _prev = self.sp_exec_pool.lock().insert(id, ctx);
        res
    }

    /// Handle `AuthEnableRequest`
    fn handle_auth_enable_request(
        &self,
        _req: &AuthEnableRequest,
    ) -> Result<AuthEnableResponse, ExecuteError> {
        debug!("handle_auth_enable");
        let res = Ok(AuthEnableResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        });
        if self.is_enabled() {
            debug!("auth is already enabled");
            return res;
        }
        let user = self.get_user(ROOT_USER)?;
        if user.roles.binary_search(&ROOT_ROLE.to_owned()).is_err() {
            return Err(ExecuteError::InvalidCommand(
                "root user does not have root role".to_owned(),
            ));
        }
        res
    }

    /// Handle `AuthDisableRequest`
    fn handle_auth_disable_request(&self, _req: &AuthDisableRequest) -> AuthDisableResponse {
        debug!("handle_auth_disable");
        if !self.is_enabled() {
            debug!("auth is already disabled");
        }
        AuthDisableResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        }
    }

    /// Handle `AuthStatusRequest`
    fn handle_auth_status_request(&self, _req: &AuthStatusRequest) -> AuthStatusResponse {
        debug!("handle_auth_status");
        AuthStatusResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            auth_revision: self.revision().cast(),
            enabled: self.is_enabled(),
        }
    }

    /// Handle `AuthenticateRequest`
    fn handle_authenticate_request(
        &self,
        req: &AuthenticateRequest,
    ) -> Result<AuthenticateResponse, ExecuteError> {
        debug!("handle_authenticate_request");
        if !self.is_enabled() {
            return Err(ExecuteError::InvalidCommand(
                "auth is not enabled".to_owned(),
            ));
        }
        let token = self.assign(&req.name)?;
        Ok(AuthenticateResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            token,
        })
    }

    /// Handle `AuthUserAddRequest`
    fn handle_user_add_request(
        &self,
        req: &AuthUserAddRequest,
    ) -> Result<AuthUserAddResponse, ExecuteError> {
        debug!("handle_user_add_request");
        if self.get_user(&req.name).is_ok() {
            return Err(ExecuteError::InvalidCommand(
                "user already exists".to_owned(),
            ));
        }
        Ok(AuthUserAddResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthUserGetRequest`
    fn handle_user_get_request(
        &self,
        req: &AuthUserGetRequest,
    ) -> Result<AuthUserGetResponse, ExecuteError> {
        debug!("handle_user_add_request");
        let user = self.get_user(&req.name)?;
        Ok(AuthUserGetResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            roles: user.roles,
        })
    }

    /// Handle `AuthUserListRequest`
    fn handle_user_list_request(
        &self,
        _req: &AuthUserListRequest,
    ) -> Result<AuthUserListResponse, ExecuteError> {
        debug!("handle_user_list_request");
        let users = self
            .get_all_users()?
            .into_iter()
            .map(|u| String::from_utf8_lossy(&u.name).to_string())
            .collect();
        Ok(AuthUserListResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            users,
        })
    }

    /// Handle `AuthUserDeleteRequest`
    fn handle_user_delete_request(
        &self,
        req: &AuthUserDeleteRequest,
    ) -> Result<AuthUserDeleteResponse, ExecuteError> {
        debug!("handle_user_delete_request");
        if self.is_enabled() && (req.name == ROOT_USER) {
            return Err(ExecuteError::InvalidCommand(
                "root user cannot be deleted".to_owned(),
            ));
        }
        let _user = self.get_user(&req.name)?;
        Ok(AuthUserDeleteResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthUserChangePasswordRequest`
    fn handle_user_change_password_request(
        &self,
        req: &AuthUserChangePasswordRequest,
    ) -> Result<AuthUserChangePasswordResponse, ExecuteError> {
        debug!("handle_user_change_password_request");
        let user = self.get_user(&req.name)?;
        let need_password = user.options.as_ref().map_or(true, |o| !o.no_password);
        if need_password && req.hashed_password.is_empty() {
            return Err(ExecuteError::InvalidCommand(
                "password is required but not provided".to_owned(),
            ));
        }
        Ok(AuthUserChangePasswordResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthUserGrantRoleRequest`
    fn handle_user_grant_role_request(
        &self,
        req: &AuthUserGrantRoleRequest,
    ) -> Result<AuthUserGrantRoleResponse, ExecuteError> {
        debug!("handle_user_grant_role_request");
        let _user = self.get_user(&req.user)?;
        if req.role != ROOT_ROLE {
            let _role = self.get_role(&req.role)?;
        }
        Ok(AuthUserGrantRoleResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthUserRevokeRoleRequest`
    fn handle_user_revoke_role_request(
        &self,
        req: &AuthUserRevokeRoleRequest,
    ) -> Result<AuthUserRevokeRoleResponse, ExecuteError> {
        debug!("handle_user_revoke_role_request");
        if self.is_enabled() && (req.name == ROOT_USER) && (req.role == ROOT_ROLE) {
            return Err(ExecuteError::InvalidCommand(
                "root user cannot revoke root role".to_owned(),
            ));
        }
        let user = self.get_user(&req.name)?;
        if user.roles.binary_search(&req.role).is_err() {
            return Err(ExecuteError::InvalidCommand(
                "role is not granted to the user".to_owned(),
            ));
        }
        Ok(AuthUserRevokeRoleResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthRoleAddRequest`
    fn handle_role_add_request(
        &self,
        req: &AuthRoleAddRequest,
    ) -> Result<AuthRoleAddResponse, ExecuteError> {
        debug!("handle_role_add_request");
        if self.get_role(&req.name).is_ok() {
            return Err(ExecuteError::InvalidCommand(
                "role already exists".to_owned(),
            ));
        }
        Ok(AuthRoleAddResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthRoleGetRequest`
    fn handle_role_get_request(
        &self,
        req: &AuthRoleGetRequest,
    ) -> Result<AuthRoleGetResponse, ExecuteError> {
        debug!("handle_role_get_request");
        let role = self.get_role(&req.role)?;
        let perm = if role.name == ROOT_ROLE.as_bytes() {
            vec![Permission {
                #[allow(clippy::as_conversions)] // This cast is always valid
                perm_type: Type::Readwrite as i32,
                key: vec![],
                range_end: vec![0],
            }]
        } else {
            role.key_permission
        };
        Ok(AuthRoleGetResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            perm,
        })
    }

    /// Handle `AuthRoleListRequest`
    fn handle_role_list_request(
        &self,
        _req: &AuthRoleListRequest,
    ) -> Result<AuthRoleListResponse, ExecuteError> {
        debug!("handle_role_list_request");
        let roles = self
            .get_all_roles()?
            .into_iter()
            .map(|r| String::from_utf8_lossy(&r.name).to_string())
            .collect();
        Ok(AuthRoleListResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            roles,
        })
    }

    /// Handle `UserRoleDeleteRequest`
    fn handle_role_delete_request(
        &self,
        req: &AuthRoleDeleteRequest,
    ) -> Result<AuthRoleDeleteResponse, ExecuteError> {
        debug!("handle_role_delete_request");
        if self.is_enabled() && req.role == ROOT_ROLE {
            return Err(ExecuteError::InvalidCommand(
                "root role cannot be deleted".to_owned(),
            ));
        }
        let _role = self.get_role(&req.role)?;
        Ok(AuthRoleDeleteResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthRoleGrantPermissionRequest`
    fn handle_role_grant_permission_request(
        &self,
        req: &AuthRoleGrantPermissionRequest,
    ) -> Result<AuthRoleGrantPermissionResponse, ExecuteError> {
        debug!("handle_role_grant_permission_request");
        let _role = self.get_role(&req.name)?;
        Ok(AuthRoleGrantPermissionResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Handle `AuthRoleRevokePermissionRequest`
    fn handle_role_revoke_permission_request(
        &self,
        req: &AuthRoleRevokePermissionRequest,
    ) -> Result<AuthRoleRevokePermissionResponse, ExecuteError> {
        debug!("handle_role_revoke_permission_request");
        let role = self.get_role(&req.role)?;
        if role
            .key_permission
            .binary_search_by(|p| match p.key.cmp(&req.key) {
                Ordering::Equal => p.range_end.cmp(&req.range_end),
                Ordering::Less => Ordering::Less,
                Ordering::Greater => Ordering::Greater,
            })
            .is_err()
        {
            return Err(ExecuteError::InvalidCommand(
                "permission not granted to the role".to_owned(),
            ));
        }
        Ok(AuthRoleRevokePermissionResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
        })
    }

    /// Sync `RequestWrapper`
    pub(super) fn sync_request(&self, id: &ProposeId) -> Result<i64, ExecuteError> {
        let ctx = self.sp_exec_pool.lock().remove(id).unwrap_or_else(|| {
            panic!("Failed to get speculative execution propose id {:?}", id);
        });
        if ctx.met_err() {
            return Ok(self.header_gen.revision());
        }
        let wrapper = ctx.req();
        #[allow(clippy::wildcard_enum_match_arm)]
        match wrapper {
            RequestWrapper::AuthEnableRequest(req) => {
                debug!("Sync AuthEnableRequest {:?}", req);
                self.sync_auth_enable_request(&req)?;
            }
            RequestWrapper::AuthDisableRequest(req) => {
                debug!("Sync AuthDisableRequest {:?}", req);
                self.sync_auth_disable_request(&req)?;
            }
            RequestWrapper::AuthStatusRequest(req) => {
                debug!("Sync AuthStatusRequest {:?}", req);
            }
            RequestWrapper::AuthUserAddRequest(req) => {
                debug!("Sync AuthUserAddRequest {:?}", req);
                self.sync_user_add_request(req)?;
            }
            RequestWrapper::AuthUserGetRequest(req) => {
                debug!("Sync AuthUserGetRequest {:?}", req);
            }
            RequestWrapper::AuthUserListRequest(req) => {
                debug!("Sync AuthUserListRequest {:?}", req);
            }
            RequestWrapper::AuthUserGrantRoleRequest(req) => {
                debug!("Sync AuthUserGrantRoleRequest {:?}", req);
                self.sync_user_grant_role_request(req)?;
            }
            RequestWrapper::AuthUserRevokeRoleRequest(req) => {
                debug!("Sync AuthUserRevokeRoleRequest {:?}", req);
                self.sync_user_revoke_role_request(req)?;
            }
            RequestWrapper::AuthUserChangePasswordRequest(req) => {
                debug!("Sync AuthUserChangePasswordRequest {:?}", req);
                self.sync_user_change_password_request(req)?;
            }
            RequestWrapper::AuthUserDeleteRequest(req) => {
                debug!("Sync AuthUserDeleteRequest {:?}", req);
                self.sync_user_delete_request(&req)?;
            }
            RequestWrapper::AuthRoleAddRequest(req) => {
                debug!("Sync AuthRoleAddRequest {:?}", req);
                self.sync_role_add_request(req)?;
            }
            RequestWrapper::AuthRoleGetRequest(req) => {
                debug!("Sync AuthRoleGetRequest {:?}", req);
            }
            RequestWrapper::AuthRoleGrantPermissionRequest(req) => {
                debug!("Sync AuthRoleGrantPermissionRequest {:?}", req);
                self.sync_role_grant_permission_request(req)?;
            }
            RequestWrapper::AuthRoleRevokePermissionRequest(req) => {
                debug!("Sync AuthRoleRevokePermissionRequest {:?}", req);
                self.sync_role_revoke_permission_request(&req)?;
            }
            RequestWrapper::AuthRoleListRequest(req) => {
                debug!("Sync AuthRoleListRequest {:?}", req);
            }
            RequestWrapper::AuthRoleDeleteRequest(req) => {
                debug!("Sync AuthRoleDeleteRequest {:?}", req);
                self.sync_role_delete_request(&req)?;
            }
            RequestWrapper::AuthenticateRequest(req) => {
                debug!("Sync AuthenticateRequest {:?}", req);
            }
            _ => {
                unreachable!("Other request should not be sent to this store");
            }
        }
        Ok(self.header_gen.revision())
    }

    /// Sync `AuthEnableRequest` and return whether authstore is changed.
    fn sync_auth_enable_request(&self, _req: &AuthEnableRequest) -> Result<(), ExecuteError> {
        if self.is_enabled() {
            return Ok(());
        }
        let revision = self.revision.next();
        self.put(AUTH_ENABLE_KEY.to_vec(), vec![1], revision, 0)?;
        self.enabled.store(true, AtomicOrdering::Release);
        self.create_permission_cache()
    }

    /// Sync `AuthDisableRequest` and return whether authstore is changed.
    fn sync_auth_disable_request(&self, _req: &AuthDisableRequest) -> Result<(), ExecuteError> {
        if !self.is_enabled() {
            return Ok(());
        }
        let revision = self.revision.next();
        self.put(AUTH_ENABLE_KEY.to_vec(), vec![0], revision, 0)?;
        self.enabled.store(false, AtomicOrdering::Release);
        Ok(())
    }

    /// Sync `AuthUserAddRequest` and return whether authstore is changed.
    fn sync_user_add_request(&self, req: AuthUserAddRequest) -> Result<(), ExecuteError> {
        let user = User {
            name: req.name.into_bytes(),
            password: req.hashed_password.into_bytes(),
            options: req.options,
            roles: Vec::new(),
        };
        let revision = self.revision.next();
        self.put_user(&user, revision, 0)
    }

    /// Sync `AuthUserDeleteRequest` and return whether authstore is changed.
    fn sync_user_delete_request(&self, req: &AuthUserDeleteRequest) -> Result<(), ExecuteError> {
        let next_revision = self.revision.next();
        self.delete_user(&req.name, next_revision, 0)?;
        self.permission_cache.map_write(|mut cache| {
            let _ignore = cache.user_permissions.remove(&req.name);
            cache.role_to_users_map.iter_mut().for_each(|(_, users)| {
                if let Some((idx, _)) = users.iter().find_position(|uname| uname == &&req.name) {
                    let _old = users.swap_remove(idx);
                };
            });
        });
        Ok(())
    }

    /// Sync `AuthUserChangePasswordRequest` and return whether authstore is changed.
    fn sync_user_change_password_request(
        &self,
        req: AuthUserChangePasswordRequest,
    ) -> Result<(), ExecuteError> {
        let mut user = self.get_user(&req.name)?;
        user.password = req.hashed_password.into_bytes();
        let revision = self.revision.next();
        self.put_user(&user, revision, 0)
    }

    /// Sync `AuthUserGrantRoleRequest` and return whether authstore is changed.
    fn sync_user_grant_role_request(
        &self,
        req: AuthUserGrantRoleRequest,
    ) -> Result<(), ExecuteError> {
        let mut user = self.get_user(&req.user)?;
        let role = self.get_role(&req.role);
        if (req.role != ROOT_ROLE) && role.is_err() {
            return Err(ExecuteError::InvalidCommand(format!(
                "Role {} does not exist",
                req.role
            )));
        }
        let idx = match user.roles.binary_search(&req.role) {
            Ok(_) => {
                return Err(ExecuteError::InvalidCommand(format!(
                    "User {} already has role {}",
                    req.user, req.role
                )));
            }
            Err(idx) => idx,
        };
        user.roles.insert(idx, req.role.clone());
        let revision = self.revision.next();
        self.put_user(&user, revision, 0)?;
        if let Ok(role) = role {
            let perms = role.key_permission;
            self.permission_cache.map_write(|mut cache| {
                let entry = cache
                    .user_permissions
                    .entry(req.user.clone())
                    .or_insert_with(UserPermissions::new);
                for perm in perms {
                    let key_range = KeyRange::new(perm.key, perm.range_end);
                    #[allow(clippy::unwrap_used)] // safe unwrap
                    match Type::from_i32(perm.perm_type).unwrap() {
                        Type::Readwrite => {
                            entry.read.push(key_range.clone());
                            entry.write.push(key_range);
                        }
                        Type::Write => {
                            entry.write.push(key_range);
                        }
                        Type::Read => {
                            entry.read.push(key_range);
                        }
                    }
                }
                cache
                    .role_to_users_map
                    .entry(req.role)
                    .or_insert_with(Vec::new)
                    .push(req.user);
            });
        }
        Ok(())
    }

    /// Sync `AuthUserRevokeRoleRequest` and return whether authstore is changed.
    fn sync_user_revoke_role_request(
        &self,
        req: AuthUserRevokeRoleRequest,
    ) -> Result<(), ExecuteError> {
        let mut user = self.get_user(&req.name)?;
        let idx = user.roles.binary_search(&req.role).map_err(|_ignore| {
            ExecuteError::InvalidCommand(format!(
                "User {} does not have role {}",
                req.name, req.role
            ))
        })?;
        let _ignore = user.roles.remove(idx);
        let revision = self.revision.next();
        self.put_user(&user, revision, 0)?;
        self.permission_cache.map_write(|mut cache| {
            let user_permissions = self.get_user_permissions(&user);
            let _entry = cache.role_to_users_map.entry(req.role).and_modify(|users| {
                if let Some((i, _)) = users.iter().find_position(|uname| uname == &&req.name) {
                    let _old = users.swap_remove(i);
                };
            });
            let _old = cache.user_permissions.insert(req.name, user_permissions);
        });
        Ok(())
    }

    /// Sync `AuthRoleAddRequest` and return whether authstore is changed.
    fn sync_role_add_request(&self, req: AuthRoleAddRequest) -> Result<(), ExecuteError> {
        let role = Role {
            name: req.name.into_bytes(),
            key_permission: Vec::new(),
        };
        let revision = self.revision.next();
        self.put_role(&role, revision, 0)
    }

    /// Sync `AuthRoleDeleteRequest` and return whether authstore is changed.
    fn sync_role_delete_request(&self, req: &AuthRoleDeleteRequest) -> Result<(), ExecuteError> {
        let revision = self.revision.next();
        self.delete_role(&req.role, revision, 0)?;
        let users = self.get_all_users()?;
        let mut sub_revision = 1;
        let mut new_perms = HashMap::new();
        for mut user in users {
            if let Ok(idx) = user.roles.binary_search(&req.role) {
                let _ignore = user.roles.remove(idx);
                self.put_user(&user, revision, sub_revision)?;
                sub_revision = sub_revision.wrapping_add(1);
                let perms = self.get_user_permissions(&user);
                let _old = new_perms.insert(String::from_utf8_lossy(&user.name).to_string(), perms);
            }
        }
        self.permission_cache.map_write(|mut cache| {
            cache.user_permissions.extend(new_perms.into_iter());
            let _ignore = cache.role_to_users_map.remove(&req.role);
        });
        Ok(())
    }

    /// Sync `AuthRoleGrantPermissionRequest` and return whether authstore is changed.
    fn sync_role_grant_permission_request(
        &self,
        req: AuthRoleGrantPermissionRequest,
    ) -> Result<(), ExecuteError> {
        let mut role = self.get_role(&req.name)?;
        let permission = req.perm.ok_or_else(|| {
            ExecuteError::InvalidCommand("Permission is not specified".to_owned())
        })?;

        #[allow(clippy::indexing_slicing)] // this index is always valid
        match role
            .key_permission
            .binary_search_by(|p| match p.key.cmp(&permission.key) {
                Ordering::Equal => p.range_end.cmp(&permission.range_end),
                Ordering::Less => Ordering::Less,
                Ordering::Greater => Ordering::Greater,
            }) {
            Ok(idx) => {
                role.key_permission[idx].perm_type = permission.perm_type;
            }
            Err(idx) => {
                role.key_permission.insert(idx, permission.clone());
            }
        };
        let revision = self.revision.next();
        self.put_role(&role, revision, 0)?;
        self.permission_cache.map_write(move |mut cache| {
            let users = cache
                .role_to_users_map
                .get(&req.name)
                .cloned()
                .unwrap_or_default();
            let key_range = KeyRange::new(permission.key, permission.range_end);
            for user in users {
                let entry = cache
                    .user_permissions
                    .entry(user)
                    .or_insert_with(UserPermissions::new);
                #[allow(clippy::unwrap_used)] // safe unwrap
                match Type::from_i32(permission.perm_type).unwrap() {
                    Type::Readwrite => {
                        entry.read.push(key_range.clone());
                        entry.write.push(key_range.clone());
                    }
                    Type::Write => {
                        entry.write.push(key_range.clone());
                    }
                    Type::Read => {
                        entry.read.push(key_range.clone());
                    }
                }
            }
        });
        Ok(())
    }

    /// Sync `AuthRoleRevokePermissionRequest` and return whether authstore is changed.
    fn sync_role_revoke_permission_request(
        &self,
        req: &AuthRoleRevokePermissionRequest,
    ) -> Result<(), ExecuteError> {
        let mut role = self.get_role(&req.role)?;
        let idx = role
            .key_permission
            .binary_search_by(|p| match p.key.cmp(&req.key) {
                Ordering::Equal => p.range_end.cmp(&req.range_end),
                Ordering::Less => Ordering::Less,
                Ordering::Greater => Ordering::Greater,
            })
            .map_err(|_ignore| ExecuteError::InvalidCommand("Permission not found".to_owned()))?;
        let _ignore = role.key_permission.remove(idx);
        let next_revision = self.revision.next();
        self.put_role(&role, next_revision, 0)?;
        self.permission_cache.map_write(|mut cache| {
            let users = cache
                .role_to_users_map
                .get(&req.role)
                .map_or_else(Vec::new, |users| {
                    users
                        .iter()
                        .filter_map(|user| self.get_user(user).ok())
                        .collect::<Vec<_>>()
                });
            for user in users {
                let perms = self.get_user_permissions(&user);
                let _old = cache
                    .user_permissions
                    .insert(String::from_utf8_lossy(&user.name).to_string(), perms);
            }
        });
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn permission_cache(&self) -> PermissionCache {
        self.permission_cache.map_read(|cache| cache.clone())
    }
}
