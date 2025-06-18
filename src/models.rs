// src/models.rs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// === Database Models ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DidRecord {
    pub id: Uuid,
    pub did: String,
    #[serde(rename = "document")] // Store as "document" in bincode
    pub document_json_string: String,
    #[serde(skip)] // Do not serialize/deserialize this field directly
    pub document_val: serde_json::Value, // Transient field to hold parsed JSON Value

    pub last_operation_cid: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_tombstoned: bool,
}

impl DidRecord {
    pub fn new(did: String, document: serde_json::Value) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            did,
            document_json_string: serde_json::to_string(&document).unwrap_or_else(|_| "{}".to_string()), // Store as string
            document_val: document, // Keep parsed value in transient field
            last_operation_cid: None,
            created_at: now,
            updated_at: now,
            is_tombstoned: false,
        }
    }

    pub fn tombstone(&mut self) {
        self.is_tombstoned = true;
        self.updated_at = Utc::now();
    }

    pub fn update_document(&mut self, document: serde_json::Value, operation_cid: Option<String>) {
        self.document_val = document;
        self.document_json_string = serde_json::to_string(&self.document_val).unwrap_or_else(|_| "{}".to_string()); // Update string
        self.last_operation_cid = operation_cid;
        self.updated_at = Utc::now();
        self.is_tombstoned = false;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: Uuid,
    pub did: String,
    pub operation: serde_json::Value,
    pub cid: String,
    pub nullified: bool,
    pub created_at: DateTime<Utc>,
    pub indexed_at: DateTime<Utc>,
}

impl OperationRecord {
    pub fn new(did: String, operation: serde_json::Value, cid: String, created_at: DateTime<Utc>) -> Self {
        Self {
            id: Uuid::new_v4(),
            did,
            operation,
            cid,
            nullified: false,
            created_at,
            indexed_at: Utc::now(),
        }
    }

    pub fn nullify(&mut self) {
        self.nullified = true;
        self.indexed_at = Utc::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScraperJob {
    pub id: Uuid,
    pub job_type: String,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub records_processed: i64,
    pub last_cursor: Option<String>,
}

impl ScraperJob {
    pub fn new(job_type: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            job_type,
            status: "pending".to_string(),
            started_at: Utc::now(),
            completed_at: None,
            error_message: None,
            records_processed: 0,
            last_cursor: None,
        }
    }

    pub fn start(&mut self) {
        self.status = "running".to_string();
        self.started_at = Utc::now();
    }

    pub fn complete(&mut self) {
        self.status = "completed".to_string();
        self.completed_at = Some(Utc::now());
    }

    pub fn fail(&mut self, error: String) {
        self.status = "failed".to_string();
        self.completed_at = Some(Utc::now());
        self.error_message = Some(error);
    }

    pub fn update_progress(&mut self, records_processed: i64, cursor: Option<String>) {
        self.records_processed = records_processed;
        self.last_cursor = cursor;
    }
}

// API Response Types

#[derive(Debug, Serialize, Deserialize)]
pub struct PlcOperation {
    #[serde(rename = "type")]
    pub op_type: String,
    #[serde(rename = "rotationKeys", skip_serializing_if = "Option::is_none")]
    pub rotation_keys: Option<Vec<String>>,
    #[serde(rename = "verificationMethods", skip_serializing_if = "Option::is_none")]
    pub verification_methods: Option<serde_json::Value>,
    #[serde(rename = "alsoKnownAs", skip_serializing_if = "Option::is_none")]
    pub also_known_as: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<serde_json::Value>,
    pub prev: Option<String>,
    pub sig: String,

    // Legacy create operation fields
    #[serde(rename = "signingKey", skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    #[serde(rename = "recoveryKey", skip_serializing_if = "Option::is_none")]
    pub recovery_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

impl PlcOperation {
    pub fn is_tombstone(&self) -> bool {
        self.op_type == "plc_tombstone"
    }

    pub fn is_legacy_create(&self) -> bool {
        self.op_type == "create"
    }

    pub fn is_plc_operation(&self) -> bool {
        self.op_type == "plc_operation"
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub did: String,
    pub operation: PlcOperation,
    pub cid: String,
    pub nullified: bool,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<DateTime<Utc>>,
}

impl Default for ExportQuery {
    fn default() -> Self {
        Self {
            count: Some(1000),
            after: None,
        }
    }
}

// === Storage Keys for Fjall ===

pub mod storage_keys {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    pub fn did_key(did: &str) -> Vec<u8> {
        format!("did:{}", did).into_bytes()
    }

    pub fn operation_key(cid: &str) -> Vec<u8> {
        format!("op:{}", cid).into_bytes()
    }

    pub fn operation_by_did_key(did: &str, indexed_at: DateTime<Utc>) -> Vec<u8> {
        format!("did_ops:{}:{}", did, indexed_at.timestamp_millis()).into_bytes()
    }

    pub fn job_key(job_id: &Uuid) -> Vec<u8> {
        format!("job:{}", job_id).into_bytes()
    }

    pub fn stats_key() -> Vec<u8> {
        b"stats".to_vec()
    }

    pub fn did_index_key(updated_at: DateTime<Utc>, did: &str) -> Vec<u8> {
        format!("did_idx:{}:{}", updated_at.timestamp_millis(), did).into_bytes()
    }

    pub fn operation_index_key(indexed_at: DateTime<Utc>, cid: &str) -> Vec<u8> {
        format!("op_idx:{}:{}", indexed_at.timestamp_millis(), cid).into_bytes()
    }

    // Add keys for settings, scraper config, and peers
    pub fn settings_key() -> Vec<u8> {
        b"system_settings".to_vec()
    }

    pub fn scraper_config_key() -> Vec<u8> {
        b"scraper_config".to_vec()
    }

    pub fn peer_key(peer_id: &str) -> Vec<u8> {
        format!("peer:{}", peer_id).into_bytes()
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub total_dids: i64,
    pub total_operations: i64,
    pub last_sync: Option<DateTime<Utc>>,
    pub average_ops_per_did: String, // <--- CONFIRM THIS IS 'String'
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            total_dids: 0,
            total_operations: 0,
            last_sync: None,
            average_ops_per_did: "0.00".to_string(), // Ensure default is also a String
        }
    }
}

// Iroh Peers, System Settings, and Scraper Configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub address: String,
    pub last_heard: DateTime<Utc>,
    pub latency_ms: u32,
}

impl Peer {
    pub fn new(id: String, address: String, latency_ms: u32) -> Self {
        Self {
            id,
            address,
            last_heard: Utc::now(),
            latency_ms,
        }
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemSettings {
    pub plc_directory_url: String,
    pub bluesky_pds_url: String,
    pub storage_type: String, // e.g., "SQLite", "PostgreSQL", "Amazon S3". For now just fjall
    pub sqlite_db_path: String,
    pub enable_email_notifications: bool,
    pub notification_email_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScraperConfig {
    pub scraper_mode: String, // "http" or "iroh"
    pub run_frequency: String, // "every_15_minutes", "daily_00_00_utc", "manual_only"
    pub scraper_enabled: bool,
}
