//! Low-level Google Drive API v3 HTTP client.
//!
//! All methods that require authentication call `access_token()` first so
//! expired access tokens are silently refreshed.
//!
//! ## Shared Drive support
//!
//! When `shared_drive_id` is `Some`, all listing calls include the required
//! `driveId`, `corpora=drive`, and `includeItemsFromAllDrives=true` parameters.
//! All mutation calls include `supportsAllDrives=true`.
//!
//! ## Service Account refresh
//!
//! When `sa_key` is `Some`, an expired token is renewed by signing a fresh JWT
//! assertion instead of using a refresh token.

use super::auth::{ServiceAccountKey, Token};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Mutex;

const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str = "id,name,mimeType,size,modifiedTime,parents";

// ---------------------------------------------------------------------------
// API response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// `None` for Google-native formats (Docs, Sheets …).
    pub size: Option<String>,
    #[serde(rename = "modifiedTime")]
    pub modified_time: Option<String>,
    #[allow(dead_code)]
    pub parents: Option<Vec<String>>,
}

impl DriveFile {
    pub fn is_dir(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    pub fn size_bytes(&self) -> u64 {
        self.size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    pub fn mtime_secs(&self) -> u64 {
        self.modified_time
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Drive HTTP client
// ---------------------------------------------------------------------------

/// Thread-safe wrapper around a `reqwest` blocking client, an OAuth2 / service
/// account token, and optional Shared Drive context.
pub struct DriveHttp {
    http: reqwest::blocking::Client,
    token: Mutex<Token>,
    /// When `Some`, expired tokens are renewed by re-signing a JWT assertion
    /// instead of exchanging a refresh token.
    sa_key: Option<ServiceAccountKey>,
    /// When `Some`, all list/mutation calls include `supportsAllDrives=true`
    /// and the necessary Shared Drive query parameters.
    shared_drive_id: Option<String>,
}

impl DriveHttp {
    /// Construct a client for the OAuth2 (installed-app) flow.
    pub fn new(
        http: reqwest::blocking::Client,
        token: Token,
        shared_drive_id: Option<String>,
    ) -> Self {
        Self {
            http,
            token: Mutex::new(token),
            sa_key: None,
            shared_drive_id,
        }
    }

    /// Construct a client for a service account.
    pub fn new_service_account(
        http: reqwest::blocking::Client,
        token: Token,
        sa_key: ServiceAccountKey,
        shared_drive_id: Option<String>,
    ) -> Self {
        Self {
            http,
            token: Mutex::new(token),
            sa_key: Some(sa_key),
            shared_drive_id,
        }
    }

    pub(super) fn shared_drive(&self) -> Option<&str> {
        self.shared_drive_id.as_deref()
    }

    /// Set the account email on the stored token and persist it (OAuth2 only).
    pub fn set_email(&self, email: &str) {
        let mut t = self.token.lock().unwrap();
        if t.email != email {
            t.email = email.to_string();
            if self.sa_key.is_none() {
                if let Err(e) = super::auth::save_token(&t) {
                    log::warn!("failed to persist token after setting email: {e}");
                }
            }
        }
    }

    fn access_token(&self) -> Result<String> {
        let mut t = self.token.lock().unwrap();
        if t.is_expired() {
            if let Some(ref key) = self.sa_key {
                // Service account: re-sign a fresh JWT assertion.
                let new = super::auth::obtain_service_account_token(key, &self.http)
                    .context("refresh service account token")?;
                t.access_token = new.access_token;
                t.expires_at = new.expires_at;
            } else {
                t.refresh(&self.http).context("refresh OAuth2 token")?;
                if let Err(e) = super::auth::save_token(&t) {
                    log::warn!("failed to persist refreshed token: {e}");
                }
            }
        }
        Ok(t.access_token.clone())
    }

    fn auth_get(&self, url: &str) -> Result<reqwest::blocking::RequestBuilder> {
        Ok(self.http.get(url).bearer_auth(self.access_token()?))
    }

    fn auth_post(&self, url: &str) -> Result<reqwest::blocking::RequestBuilder> {
        Ok(self.http.post(url).bearer_auth(self.access_token()?))
    }

    fn auth_patch(&self, url: &str) -> Result<reqwest::blocking::RequestBuilder> {
        Ok(self.http.patch(url).bearer_auth(self.access_token()?))
    }

    fn auth_delete(&self, url: &str) -> Result<reqwest::blocking::RequestBuilder> {
        Ok(self.http.delete(url).bearer_auth(self.access_token()?))
    }

    // -----------------------------------------------------------------------
    // File metadata
    // -----------------------------------------------------------------------

    /// Return the email address of the authenticated account.
    pub fn account_email(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct About {
            user: User,
        }
        #[derive(Deserialize)]
        struct User {
            #[serde(rename = "emailAddress")]
            email: String,
        }

        let resp = self
            .auth_get(&format!("{API_BASE}/about"))?
            .query(&[("fields", "user/emailAddress")])
            .send()
            .context("about.get")?;

        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!(
                "Google Drive API returned 403 Forbidden.\n\
                 \n\
                 This usually means one of:\n\
                 \n\
                 1. The Google Drive API is not enabled for your Cloud project.\n\
                    Enable it at: https://console.cloud.google.com/apis/library/drive.googleapis.com\n\
                 \n\
                 2. Your cached token was issued before the Drive scope was granted.\n\
                    Delete it and re-authorize:\n\
                    rm ~/.config/remotefs/gdrive_token_*.json"
            );
        }

        let about: About = resp
            .error_for_status()
            .context("about.get status")?
            .json()
            .context("parse about.get")?;
        Ok(about.user.email)
    }

    /// Return `(total_bytes, free_bytes)` from the account's storage quota.
    ///
    /// `limit` is absent for Google Workspace accounts with pooled/unlimited
    /// storage; in that case `total_bytes` is returned as 0.
    pub fn storage_quota(&self) -> Result<(u64, u64)> {
        #[derive(Deserialize)]
        struct About {
            #[serde(rename = "storageQuota")]
            quota: Quota,
        }
        #[derive(Deserialize)]
        struct Quota {
            limit: Option<String>,
            usage: String,
        }

        let about: About = self
            .auth_get(&format!("{API_BASE}/about"))?
            .query(&[("fields", "storageQuota")])
            .send()
            .context("about.get quota")?
            .error_for_status()
            .context("about.get quota status")?
            .json()
            .context("parse about.get quota")?;

        let used: u64 = about.quota.usage.parse().unwrap_or(0);
        let total: u64 = about.quota.limit.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((total, total.saturating_sub(used)))
    }

    pub fn get_file(&self, file_id: &str) -> Result<DriveFile> {
        self.auth_get(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[("fields", FILE_FIELDS), ("supportsAllDrives", "true")])
            .send()
            .context("files.get")?
            .error_for_status()
            .context("files.get status")?
            .json::<DriveFile>()
            .context("parse files.get")
    }

    /// Paginate `files.list` for the given query and return all results.
    fn list_files(&self, query: &str, context: &str) -> Result<Vec<DriveFile>> {
        #[derive(Deserialize)]
        struct ListResp {
            files: Vec<DriveFile>,
            #[serde(rename = "nextPageToken")]
            next_page_token: Option<String>,
        }

        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut req = self
                .auth_get(&format!("{API_BASE}/files"))?
                .query(&[
                    ("q", query),
                    ("fields", fields.as_str()),
                    ("pageSize", "1000"),
                    ("supportsAllDrives", "true"),
                    ("includeItemsFromAllDrives", "true"),
                ]);
            if let Some(ref drive_id) = self.shared_drive_id {
                req = req.query(&[("corpora", "drive"), ("driveId", drive_id.as_str())]);
            }
            if let Some(ref pt) = page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }

            let resp: ListResp = req
                .send()
                .with_context(|| context.to_string())?
                .error_for_status()
                .with_context(|| format!("{context} status"))?
                .json()
                .with_context(|| format!("parse {context}"))?;

            all.extend(resp.files);
            match resp.next_page_token {
                Some(pt) => page_token = Some(pt),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all non-trashed children of `folder_id`.
    pub fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>> {
        let q = format!("'{folder_id}' in parents and trashed = false");
        self.list_files(&q, "files.list")
    }

    /// List all trashed files visible to the authenticated account.
    ///
    /// For Shared Drives, results are scoped to `shared_drive_id`.
    pub fn list_trashed(&self) -> Result<Vec<DriveFile>> {
        self.list_files("trashed = true", "list trashed")
    }

    // -----------------------------------------------------------------------
    // File content
    // -----------------------------------------------------------------------

    pub fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        let bytes = self
            .auth_get(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[("alt", "media"), ("supportsAllDrives", "true")])
            .send()
            .context("files.get media")?
            .error_for_status()
            .context("files.get media status")?
            .bytes()
            .context("read file content")?;
        Ok(bytes.to_vec())
    }

    /// Upload new content for an existing file (simple media upload).
    pub fn upload(&self, file_id: &str, content: Vec<u8>) -> Result<()> {
        self.auth_patch(&format!("{UPLOAD_BASE}/files/{file_id}"))?
            .query(&[("uploadType", "media"), ("supportsAllDrives", "true")])
            .header("Content-Type", "application/octet-stream")
            .body(content)
            .send()
            .context("files.update media")?
            .error_for_status()
            .context("files.update media status")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------------

    pub fn create_file(&self, parent_id: &str, name: &str) -> Result<DriveFile> {
        #[derive(serde::Serialize)]
        struct Meta<'a> {
            name: &'a str,
            parents: [&'a str; 1],
        }
        let meta = serde_json::to_string(&Meta {
            name,
            parents: [parent_id],
        })?;

        let boundary = "---drive_boundary_remotefs";
        let body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n\
             --{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n\r\n\
             --{boundary}--"
        );

        self.auth_post(&format!("{UPLOAD_BASE}/files"))?
            .query(&[
                ("uploadType", "multipart"),
                ("fields", FILE_FIELDS),
                ("supportsAllDrives", "true"),
            ])
            .header(
                "Content-Type",
                format!("multipart/related; boundary={boundary}"),
            )
            .body(body)
            .send()
            .context("files.create")?
            .error_for_status()
            .context("files.create status")?
            .json::<DriveFile>()
            .context("parse files.create")
    }

    pub fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveFile> {
        let body = serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MIME,
            "parents": [parent_id],
        });

        self.auth_post(&format!("{API_BASE}/files"))?
            .query(&[("fields", FILE_FIELDS), ("supportsAllDrives", "true")])
            .json(&body)
            .send()
            .context("folder create")?
            .error_for_status()
            .context("folder create status")?
            .json::<DriveFile>()
            .context("parse folder create")
    }

    /// Move a file to the trash (recoverable). Use `delete_permanent` to skip trash.
    pub fn trash_file(&self, file_id: &str) -> Result<()> {
        self.auth_patch(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[("supportsAllDrives", "true"), ("fields", "id")])
            .json(&serde_json::json!({"trashed": true}))
            .send()
            .context("trash_file")?
            .error_for_status()
            .context("trash_file status")?;
        Ok(())
    }

    /// Restore a trashed file, move it to `new_parent_id`, and rename it.
    ///
    /// Fetches the file's current parent list in order to remove them atomically
    /// in the same `files.update` call.
    pub fn restore_file(&self, file_id: &str, new_parent_id: &str, new_name: &str) -> Result<()> {
        let file = self.get_file(file_id)?;
        let remove_parents = file.parents.unwrap_or_default().join(",");

        let req = self
            .auth_patch(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[
                ("supportsAllDrives", "true"),
                ("addParents", new_parent_id),
                ("fields", "id"),
            ]);
        let req = if remove_parents.is_empty() {
            req
        } else {
            req.query(&[("removeParents", remove_parents.as_str())])
        };
        req.json(&serde_json::json!({"trashed": false, "name": new_name}))
            .send()
            .context("restore_file")?
            .error_for_status()
            .context("restore_file status")?;
        Ok(())
    }

    /// Permanently delete a file, bypassing the trash.
    pub fn delete_permanent(&self, file_id: &str) -> Result<()> {
        self.auth_delete(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[("supportsAllDrives", "true")])
            .send()
            .context("files.delete")?
            .error_for_status()
            .context("files.delete status")?;
        Ok(())
    }

    /// Rename a file and/or move it to a different parent folder.
    pub fn rename(
        &self,
        file_id: &str,
        new_name: &str,
        old_parent: &str,
        new_parent: &str,
    ) -> Result<()> {
        let mut req = self
            .auth_patch(&format!("{API_BASE}/files/{file_id}"))?
            .query(&[
                ("supportsAllDrives", "true"),
                ("addParents", new_parent),
                ("removeParents", old_parent),
                ("fields", "id"),
            ]);

        if !new_name.is_empty() {
            req = req.json(&serde_json::json!({ "name": new_name }));
        }

        req.send()
            .context("files.update rename")?
            .error_for_status()
            .context("files.update rename status")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RFC 3339 timestamp parser (avoids a chrono dependency)
// ---------------------------------------------------------------------------

fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let s = s.trim_end_matches("+00:00");
    let (date, time) = s.split_once('T')?;

    let mut dp = date.splitn(3, '-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;

    let mut tp = time.splitn(3, ':');
    let hour: u64 = tp.next()?.parse().ok()?;
    let min: u64 = tp.next()?.parse().ok()?;
    let sec_str = tp.next().unwrap_or("0");
    let sec: u64 = sec_str.split('.').next()?.parse().ok()?;

    let days = civil_to_days(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

fn civil_to_days(year: i64, month: u32, day: u32) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let rd = era * 146_097 + doe - 719_468;
    u64::try_from(rd).ok()
}
