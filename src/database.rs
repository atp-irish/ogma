use crate::models::{
    storage_keys, DidRecord, LogEntry, OperationRecord, Peer, PlcOperation, ScraperConfig, ScraperJob, Stats, SystemSettings,
};
use anyhow::{Context, Result};
use fjall::{Config, Keyspace, PartitionHandle, PersistMode};
use std::path::Path;
use uuid::Uuid;
use chrono::Utc;
use tracing::{debug, error, warn};
use hex; 

pub struct Database {
    keyspace: Keyspace,
    dids: PartitionHandle,
    operations: PartitionHandle,
    jobs: PartitionHandle,
    stats: PartitionHandle,
    settings: PartitionHandle, 
    scraper_config: PartitionHandle, 
    peers: PartitionHandle,    
    did_index: PartitionHandle,     
    operation_index: PartitionHandle, 
    operation_by_did: PartitionHandle, 
}

impl Database {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        tracing::info!("Opening fjall database at: {:?}", data_dir.as_ref());

        let keyspace = Config::new(data_dir).open()?;

        let dids = keyspace.open_partition("dids", Default::default())?;
        let operations = keyspace.open_partition("operations", Default::default())?;
        let jobs = keyspace.open_partition("jobs", Default::default())?;
        let stats = keyspace.open_partition("stats", Default::default())?;
        let settings = keyspace.open_partition("settings", Default::default())?;
        let scraper_config = keyspace.open_partition("scraper_config", Default::default())?;
        let peers = keyspace.open_partition("peers", Default::default())?;
        let did_index = keyspace.open_partition("did_index", Default::default())?;
        let operation_index = keyspace.open_partition("operation_index", Default::default())?;
        let operation_by_did = keyspace.open_partition("operation_by_did", Default::default())?;

        Ok(Self {
            keyspace,
            dids,
            operations,
            jobs,
            stats,
            settings,
            scraper_config,
            peers,
            did_index,
            operation_index,
            operation_by_did,
        })
    }

    pub fn flush(&self) -> Result<()> {
        self.keyspace.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    // === Stats Methods ===

    pub fn get_stats(&self) -> Result<Stats> {
        let stats_bytes = self.stats.get(storage_keys::stats_key())?;
        match stats_bytes {
            Some(bytes) => {
                let (stats, _): (Stats, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                        .context("Failed to deserialize stats")?;
                Ok(stats)
            }
            None => Ok(Stats::default()),
        }
    }

    fn update_stats<F>(&self, updater: F) -> Result<()>
    where
        F: FnOnce(&mut Stats),
    {
        let mut stats = self.get_stats()?;
        updater(&mut stats);
        let stats_bytes = bincode::serde::encode_to_vec(&stats, bincode::config::standard())
            .context("Failed to serialize stats")?;
        self.stats.insert(storage_keys::stats_key(), stats_bytes)?;
        Ok(())
    }

    // === DID Methods ===

    pub fn upsert_did(&self, did_record: DidRecord) -> Result<()> { // did_record is not mut here
        let did_key = storage_keys::did_key(&did_record.did);

        let is_new = self.dids.get(&did_key)?.is_none();

        // did_record.updated_at is handled by update_document/new constructor
        let did_bytes = bincode::serde::encode_to_vec(&did_record, bincode::config::standard())
            .context("Failed to serialize DidRecord")?; // Serializes the DidRecord (now with String for document)

        self.dids.insert(&did_key, did_bytes)?; // Inserts or updates the record in the 'dids' partition

        let index_key = storage_keys::did_index_key(did_record.updated_at, &did_record.did);
        self.did_index.insert(index_key, did_record.did.as_bytes())?;

        if is_new && !did_record.is_tombstoned {
            self.update_stats(|stats| {
                stats.total_dids += 1;
            })?;
        }

        debug!("Upserted DID: {}", did_record.did);
        Ok(())
    }

    pub fn get_did(&self, did: &str) -> Result<Option<DidRecord>> {
        let did_key = storage_keys::did_key(did);
        match self.dids.get(&did_key)? {
            Some(bytes) => {
                // Added debug logging of the bytes for deserialization troubleshooting
                debug!("Attempting to deserialize DidRecord from bincode for {}. Bytes length: {}", did, bytes.len());

                // FIX: Explicitly specify the type for decode_from_slice
                match bincode::serde::decode_from_slice::<DidRecord, _>(&bytes, bincode::config::standard()) {
                    Ok((mut did_record, _)) => {
                        // FIX: Parse the stored JSON string back into serde_json::Value
                        did_record.document_val = serde_json::from_str(&did_record.document_json_string)
                            .context(format!("Failed to parse JSON string for DID {}. String: {}", did, did_record.document_json_string))?;
                        Ok(Some(did_record))
                    },
                    Err(e) => {
                        // Added more descriptive error logging
                        error!("Failed to deserialize DidRecord from bincode for {}: {}. Raw bytes (hex): {:?}", did, e, hex::encode(&bytes));
                        Err(anyhow::anyhow!("Failed to deserialize DidRecord for {}: {}", did, e)) // Re-wrap the error for context
                    }
                }
            }
            None => Ok(None),
        }
    }

    pub fn get_dids_paginated(&self, limit: usize, offset: usize) -> Result<Vec<DidRecord>> {
        let mut dids = Vec::new();
        let mut count = 0;

        // Iterate in reverse to get most recent DIDs first based on their index key
        for item in self.did_index.iter().rev() {
            let (_, did_bytes) = item?;

            // Skip records based on offset
            if count < offset {
                count += 1;
                continue;
            }

            // Limit the number of DIDs
            if dids.len() >= limit {
                break;
            }

            let did_str = String::from_utf8(did_bytes.to_vec())
                .context("Invalid DID string in index")?;

            if let Some(did_record) = self.get_did(&did_str)? {
                if !did_record.is_tombstoned {
                    dids.push(did_record);
                }
            }

            count += 1;
        }

        Ok(dids)
    }

    pub fn search_dids(&self, search_term: &str, limit: usize, offset: usize) -> Result<Vec<DidRecord>, Box<dyn std::error::Error>> {
        let search_lower = search_term.to_lowercase();
        let mut results = Vec::new();
        let mut count = 0;
        let mut skipped = 0;

        // Iterate through all DIDs and filter based on search criteria
        for item in self.dids.range::<&[u8], _>(..) {
            let (_, value) = item?;
            
            if let Ok((did_record, _)) = bincode::serde::decode_from_slice::<DidRecord, _>(&value, bincode::config::standard()) {
                // Ensure document_val is populated
                let mut record = did_record;
                if let Ok(parsed_doc) = serde_json::from_str::<serde_json::Value>(&record.document_json_string) {
                    record.document_val = parsed_doc;
                }

                // Extract handle and PDS endpoint for searching
                let handle = record.document_val
                    .get("alsoKnownAs")
                    .and_then(|aka| aka.as_array())
                    .and_then(|arr| arr.get(0))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                let pds_endpoint = record.document_val
                    .get("service")
                    .and_then(|service_array| service_array.as_array())
                    .and_then(|arr| {
                        arr.iter().find(|service| {
                            service.get("id").and_then(|id| id.as_str()) == Some("#bsky_pds")
                        })
                    })
                    .and_then(|pds_service| pds_service.get("serviceEndpoint"))
                    .and_then(|endpoint| endpoint.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                // Check if search term matches DID, handle, or PDS endpoint
                let matches = record.did.to_lowercase().contains(&search_lower) ||
                              handle.contains(&search_lower) ||
                              pds_endpoint.contains(&search_lower);

                if matches {
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    
                    if count < limit {
                        results.push(record);
                        count += 1;
                    } else {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }

    pub fn count_search_results(&self, search_term: &str) -> Result<usize, Box<dyn std::error::Error>> {
        let search_lower = search_term.to_lowercase();
        let mut count = 0;

        for item in self.dids.range::<&[u8], _>(..) {
            let (_, value) = item?;
            
            if let Ok((did_record, _)) = bincode::serde::decode_from_slice::<DidRecord, _>(&value, bincode::config::standard()) {
                let mut record = did_record;
                if let Ok(parsed_doc) = serde_json::from_str::<serde_json::Value>(&record.document_json_string) {
                    record.document_val = parsed_doc;
                }

                let handle = record.document_val
                    .get("alsoKnownAs")
                    .and_then(|aka| aka.as_array())
                    .and_then(|arr| arr.get(0))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                let pds_endpoint = record.document_val
                    .get("service")
                    .and_then(|service_array| service_array.as_array())
                    .and_then(|arr| {
                        arr.iter().find(|service| {
                            service.get("id").and_then(|id| id.as_str()) == Some("#bsky_pds")
                        })
                    })
                    .and_then(|pds_service| pds_service.get("serviceEndpoint"))
                    .and_then(|endpoint| endpoint.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                if record.did.to_lowercase().contains(&search_lower) ||
                   handle.contains(&search_lower) ||
                   pds_endpoint.contains(&search_lower) {
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    // === Operation Methods ===

    pub fn upsert_operation(&self, mut operation_record: OperationRecord) -> Result<()> {
        let op_key = storage_keys::operation_key(&operation_record.cid);

        let is_new = self.operations.get(&op_key)?.is_none();

        operation_record.indexed_at = Utc::now();
        let op_bytes = bincode::serde::encode_to_vec(&operation_record, bincode::config::standard())
            .context("Failed to serialize operation record")?;

        self.operations.insert(&op_key, op_bytes)?;

        let index_key = storage_keys::operation_index_key(operation_record.indexed_at, &operation_record.cid);
        self.operation_index.insert(index_key, operation_record.cid.as_bytes())?;

        let did_op_key = storage_keys::operation_by_did_key(&operation_record.did, operation_record.indexed_at);
        self.operation_by_did.insert(did_op_key, operation_record.cid.as_bytes())?;

        if is_new {
            self.update_stats(|stats| {
                stats.total_operations += 1;
            })?;
        }

        debug!("Upserted operation: {} for DID: {}", operation_record.cid, operation_record.did);
        Ok(())
    }

    pub fn get_operation(&self, cid: &str) -> Result<Option<OperationRecord>> {
        let op_key = storage_keys::operation_key(cid);
        match self.operations.get(&op_key)? {
            Some(bytes) => {
                let (operation_record, _): (OperationRecord, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                        .context("Failed to deserialize operation record")?;
                Ok(Some(operation_record))
            }
            None => Ok(None),
        }
    }

    pub fn get_operations_paginated(&self, limit: usize, offset: usize) -> Result<Vec<OperationRecord>> {
        let mut operations = Vec::new();
        let mut count = 0;

        for item in self.operation_index.iter().rev() {
            let (_, cid_bytes) = item?;

            if count < offset {
                count += 1;
                continue;
            }

            if operations.len() >= limit {
                break;
            }

            let cid = String::from_utf8(cid_bytes.to_vec())
                .context("Invalid CID string in index")?;

            if let Some(operation_record) = self.get_operation(&cid)? {
                operations.push(operation_record);
            }

            count += 1;
        }

        Ok(operations)
    }

    pub fn get_did_operations(&self, did: &str) -> Result<Vec<OperationRecord>> {
        let mut operations = Vec::new();
        let prefix = format!("did_ops:{}:", did);

        for item in self.operation_by_did.prefix(prefix.as_bytes()) {
            let (_, cid_bytes) = item?;
            let cid = String::from_utf8(cid_bytes.to_vec())
                .context("Invalid CID string in operation index")?;

            if let Some(operation_record) = self.get_operation(&cid)? {
                if !operation_record.nullified {
                    operations.push(operation_record);
                }
            }
        }

        operations.sort_by(|a, b| a.indexed_at.cmp(&b.indexed_at));

        Ok(operations)
    }

    // === Job Methods ===

    pub fn insert_job(&self, job: &ScraperJob) -> Result<()> {
        let job_key = storage_keys::job_key(&job.id);
        let job_bytes = bincode::serde::encode_to_vec(job, bincode::config::standard())
            .context("Failed to serialize scraper job")?;
        self.jobs.insert(job_key, job_bytes)?;
        debug!("Inserted job: {} ({})", job.id, job.job_type);
        Ok(())
    }

    pub fn update_job(&self, job: &ScraperJob) -> Result<()> {
        let job_key = storage_keys::job_key(&job.id);
        let job_bytes = bincode::serde::encode_to_vec(job, bincode::config::standard())
            .context("Failed to serialize scraper job")?;
        self.jobs.insert(job_key, job_bytes)?;
        debug!("Updated job: {} (status: {})", job.id, job.status);
        Ok(())
    }

    pub fn get_job(&self, job_id: &Uuid) -> Result<Option<ScraperJob>> {
        let job_key = storage_keys::job_key(job_id);
        match self.jobs.get(&job_key)? {
            Some(bytes) => {
                let (job, _): (ScraperJob, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                        .context("Failed to deserialize scraper job")?;
                Ok(Some(job))
            }
            None => Ok(None),
        }
    }

    pub fn get_active_jobs(&self) -> Result<Vec<ScraperJob>> {
        let mut active_jobs = Vec::new();

        for item in self.jobs.iter() {
            let (_, job_bytes) = item?;
            let (job, _): (ScraperJob, _) =
                bincode::serde::decode_from_slice(&job_bytes, bincode::config::standard())
                    .context("Failed to deserialize scraper job")?;

            if job.status == "running" || job.status == "pending" {
                active_jobs.push(job);
            }
        }

        active_jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        Ok(active_jobs)
    }

    pub fn get_recent_jobs(&self, limit: usize) -> Result<Vec<ScraperJob>> {
        let mut jobs = Vec::new();

        for item in self.jobs.iter() {
            let (_, job_bytes) = item?;
            let (job, _): (ScraperJob, _) =
                bincode::serde::decode_from_slice(&job_bytes, bincode::config::standard())
                    .context("Failed to deserialize scraper job")?;
            jobs.push(job);
        }

        jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        jobs.truncate(limit);

        Ok(jobs)
    }

    // Returns all jobs for the history page
    pub fn get_scraper_jobs(&self) -> Result<Vec<ScraperJob>> {
        let mut all_jobs = Vec::new();
        for item in self.jobs.iter() {
            let (_, job_bytes) = item?;
            let (job, _): (ScraperJob, _) =
                bincode::serde::decode_from_slice(&job_bytes, bincode::config::standard())
                    .context("Failed to deserialize scraper job")?;
            all_jobs.push(job);
        }
        all_jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at)); // Sort by most recent
        Ok(all_jobs)
    }

    // New Methods for UI Data

    pub fn get_system_settings(&self) -> Result<SystemSettings> {
        let settings_bytes = self.settings.get(storage_keys::settings_key())?;
        match settings_bytes {
            Some(bytes) => {
                let (settings, _): (SystemSettings, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                        .context("Failed to deserialize system settings")?;
                Ok(settings)
            }
            None => {
                // If no settings are stored, return defaults and save them
                let default_settings = SystemSettings::default();
                self.update_system_settings(&default_settings)?; // Persist defaults
                Ok(default_settings)
            }
        }
    }

    pub fn update_system_settings(&self, settings: &SystemSettings) -> Result<()> {
        let settings_bytes = bincode::serde::encode_to_vec(settings, bincode::config::standard())
            .context("Failed to serialize system settings")?;
        self.settings.insert(storage_keys::settings_key(), settings_bytes)?;
        debug!("Updated system settings");
        Ok(())
    }

    pub fn get_scraper_config(&self) -> Result<ScraperConfig> {
        let config_bytes = self.scraper_config.get(storage_keys::scraper_config_key())?;
        match config_bytes {
            Some(bytes) => {
                let (config, _): (ScraperConfig, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                        .context("Failed to deserialize scraper config")?;
                Ok(config)
            }
            None => {
                // If no config is stored, return defaults and save them
                let default_config = ScraperConfig::default();
                self.update_scraper_config(&default_config)?; // Persist defaults
                Ok(default_config)
            }
        }
    }

    pub fn update_scraper_config(&self, config: &ScraperConfig) -> Result<()> {
        let config_bytes = bincode::serde::encode_to_vec(config, bincode::config::standard())
            .context("Failed to serialize scraper config")?;
        self.scraper_config.insert(storage_keys::scraper_config_key(), config_bytes)?;
        debug!("Updated scraper config");
        Ok(())
    }

    pub fn get_peers(&self) -> Result<Vec<Peer>> {
        let mut all_peers = Vec::new();
        for item in self.peers.iter() {
            let (_, peer_bytes) = item?;
            let (peer, _): (Peer, _) =
                bincode::serde::decode_from_slice(&peer_bytes, bincode::config::standard())
                    .context("Failed to deserialize peer")?;
            all_peers.push(peer);
        }
        // Sort or filter if needed, e.g., by last_heard
        Ok(all_peers)
    }

    pub fn upsert_peer(&self, peer: &Peer) -> Result<()> {
        let peer_key = storage_keys::peer_key(&peer.id);
        let peer_bytes = bincode::serde::encode_to_vec(peer, bincode::config::standard())
            .context("Failed to serialize peer")?;
        self.peers.insert(peer_key, peer_bytes)?;
        debug!("Upserted peer: {}", peer.id);
        Ok(())
    }

    // Utility Methods

    pub fn process_log_entry(&self, entry: &LogEntry) -> Result<()> {
        let operation_record = OperationRecord::new(
            entry.did.clone(),
            serde_json::to_value(&entry.operation).context("Failed to serialize operation")?,
            entry.cid.clone(),
            entry.created_at,
        );

        self.upsert_operation(operation_record)?;

        if !entry.nullified {
            self.update_did_from_operation(&entry.did, &entry.operation)?;
        }

        Ok(())
    }

    fn update_did_from_operation(&self, did: &str, operation: &PlcOperation) -> Result<()> {
        let mut document = serde_json::json!({
            "@context": ["https://www.w3.org/ns/did/v1"],
            "id": did
        });

        if operation.is_tombstone() {
            let mut did_record = self.get_did(did)?.unwrap_or_else(|| {
                // When creating a new DidRecord, ensure it uses the correct JSON Value
                DidRecord::new(did.to_string(), document.clone())
            });
            did_record.tombstone();
            did_record.document_val = document; // Update document_val
            self.upsert_did(did_record)?;
            return Ok(());
        }

        if let Some(also_known_as) = &operation.also_known_as {
            document["alsoKnownAs"] = serde_json::to_value(also_known_as)?;
        }

        if operation.is_legacy_create() {
            if let (Some(signing_key), Some(_recovery_key), Some(handle), Some(service)) = (
                &operation.signing_key,
                &operation.recovery_key,
                &operation.handle,
                &operation.service,
            ) {
                document["alsoKnownAs"] = serde_json::json!([handle]);
                document["verificationMethod"] = serde_json::json!([{
                    "id": "#atproto",
                    "type": "EcdsaSecp256k1VerificationKey2019",
                    "controller": did,
                    "publicKeyMultibase": signing_key
                }]);
                document["service"] = serde_json::json!([{
                    "id": "#atproto_pds",
                    "type": "AtprotoPersonalDataServer",
                    "serviceEndpoint": service
                }]);
            }
        } else {
            // Updated to ensure verificationMethod is always an array, even if empty
            if let Some(verification_methods) = &operation.verification_methods {
                if let serde_json::Value::Object(vm_map) = verification_methods {
                    let mut vm_array = Vec::new();
                    for (key, value) in vm_map {
                        if let serde_json::Value::String(did_key) = value {
                            vm_array.push(serde_json::json!({
                                "id": format!("#{}", key),
                                "type": "EcdsaSecp256k1VerificationKey2019",
                                "controller": did,
                                "publicKeyMultibase": did_key
                            }));
                        } else {
                            warn!("Unexpected value type in verificationMethods for DID: {}. Setting empty array for entry.", did);
                        }
                    }
                    document["verificationMethod"] = serde_json::to_value(vm_array)?;
                } else {
                    warn!("verificationMethods was not an object for DID: {}. Setting empty array for entry.", did);
                    document["verificationMethod"] = serde_json::json!([]);
                }
            } else {
                document["verificationMethod"] = serde_json::json!([]); // Ensure it's an empty array if None
            }


            if let Some(services) = &operation.services {
                if let serde_json::Value::Object(service_map) = services {
                    let mut service_array = Vec::new();
                    for (key, value) in service_map {
                        if let serde_json::Value::Object(service_obj) = value {
                            service_array.push(serde_json::json!({
                                "id": format!("#{}", key),
                                "type": service_obj.get("type").unwrap_or(&serde_json::Value::String("Unknown".to_string())),
                                "serviceEndpoint": service_obj.get("endpoint").unwrap_or(&serde_json::Value::String("".to_string()))
                            }));
                        } else {
                            warn!("Unexpected value type in services for DID: {}. Setting empty array for entry.", did);
                        }
                    }
                    document["service"] = serde_json::to_value(service_array)?;
                } else {
                    warn!("Services was not an object for DID: {}. Setting empty array for entry.", did);
                    document["service"] = serde_json::json!([]);
                }
            } else {
                document["service"] = serde_json::json!([]); // Ensure it's an empty array if None
            }
        }

        let mut did_record = self.get_did(did)?.unwrap_or_else(|| {
            DidRecord::new(did.to_string(), document.clone())
        });

        did_record.update_document(document, None); // This will update document_val and document_json_string
        self.upsert_did(did_record)?;

        Ok(())
    }

    pub fn update_last_sync(&self) -> Result<()> {
        self.update_stats(|stats| {
            stats.last_sync = Some(Utc::now());
        })?;
        Ok(())
    }
}
