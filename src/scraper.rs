use crate::database::Database;
use crate::models::{LogEntry, ScraperJob, ExportQuery};
use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::{Client, header::{HeaderMap, HeaderValue, USER_AGENT}};
use std::sync::Arc;
use tokio::time::{sleep, Duration as TokioDuration, Instant};
use tracing::{info, warn, error, debug};
use chrono::Timelike; 

#[derive(Debug, Clone)]
pub enum CircuitState {
    Closed,     // Normal operation
    Open,       // Circuit breaker tripped, rejecting requests
    HalfOpen,   // Testing if service has recovered
}

#[derive(Debug)]
pub struct CircuitBreaker {
    state: CircuitState,
    failure_count: u32,
    failure_threshold: u32,
    timeout: TokioDuration,
    last_failure_time: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, timeout: TokioDuration) -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            failure_threshold,
            timeout,
            last_failure_time: None,
        }
    }

    pub fn can_execute(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if let Some(last_failure) = self.last_failure_time {
                    if last_failure.elapsed() > self.timeout {
                        info!("Circuit breaker transitioning to half-open after timeout");
                        self.state = CircuitState::HalfOpen;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true, // Allows a single test request
        }
    }

    pub fn record_success(&mut self) {
        match self.state {
            CircuitState::HalfOpen => {
                info!("Circuit breaker closing after successful request");
                self.state = CircuitState::Closed;
                self.failure_count = 0;
                self.last_failure_time = None;
            }
            CircuitState::Closed => {
                self.failure_count = 0; // Reset on success
            }
            _ => {} // Open state stays open until timeout
        }
    }

    pub fn record_failure(&mut self) {
        self.failure_count += 1;
        self.last_failure_time = Some(Instant::now());

        match self.state {
            CircuitState::Closed => {
                if self.failure_count >= self.failure_threshold {
                    warn!("Circuit breaker opening after {} failures", self.failure_count);
                    self.state = CircuitState::Open;
                }
            }
            CircuitState::HalfOpen => {
                warn!("Circuit breaker re-opening after failure in half-open state");
                self.state = CircuitState::Open;
            }
            _ => {}
        }
    }

    pub fn get_state(&self) -> &CircuitState {
        &self.state
    }
}

pub struct Scraper {
    db: Arc<Database>,
    client: Client,
    base_url: String,
    // Rate limiting configuration
    requests_per_minute: u32,
    batch_size: usize,
    max_retries: u32,
    backoff_multiplier: f64,
    // Respect configuration
    user_agent: String,
    min_delay_between_requests: TokioDuration,
    // Circuit breaker for server error protection
    circuit_breaker: CircuitBreaker,
    // Adaptive batch sizing
    adaptive_batch_size: bool,
    min_batch_size: usize,
    max_batch_size: usize,
}

impl Scraper {
    pub fn new(db: Arc<Database>) -> Self {
        // Build a respectful HTTP client
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str("PLC-Mirror/1.0 (Respectful scraper; contact: your-email@domain.com)")
                .expect("Invalid user agent")
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(2) // Limit connection pool
            .build()
            .expect("Failed to build HTTP client");

        Self {
            db,
            client,
            base_url: "https://plc.directory".to_string(),
            // Conservative rate limiting - 30 requests per minute max
            requests_per_minute: 30,
            batch_size: 500,
            max_retries: 3,
            backoff_multiplier: 2.0,
            user_agent: "PLC-Mirror/1.0 (Respectful scraper)".to_string(),
            min_delay_between_requests: TokioDuration::from_millis(2000),
            // Circuit breaker: open after 3 server errors, timeout for 5 minutes
            circuit_breaker: CircuitBreaker::new(3, TokioDuration::from_secs(300)),
            // Adaptive batch sizing
            adaptive_batch_size: true,
            min_batch_size: 100,
            max_batch_size: 1000,
        }
    }

    /// Calculate delay between requests based on rate limiting
    fn calculate_request_delay(&self) -> TokioDuration {
        let delay_from_rate_limit = TokioDuration::from_secs(60) / self.requests_per_minute;
        std::cmp::max(delay_from_rate_limit, self.min_delay_between_requests)
    }

    /// Exponential backoff delay for retries
    fn calculate_backoff_delay(&self, attempt: u32) -> TokioDuration {
        let base_delay = 1.0; // 1 second base
        let delay_seconds = base_delay * self.backoff_multiplier.powi(attempt as i32);
        TokioDuration::from_secs_f64(delay_seconds.min(300.0)) // Cap at 5 minutes
    }

    /// Special backoff calculation for server errors (more aggressive)
    fn calculate_server_error_backoff(&self, server_error_count: u32) -> TokioDuration {
        // More aggressive backoff for server errors
        let base_delay = 5.0; // 5 second base for server errors
        let delay_seconds = base_delay * (2.0_f64).powi(server_error_count as i32);
        TokioDuration::from_secs_f64(delay_seconds.min(600.0)) // Cap at 10 minutes
    }

    /// Enhanced error handling with circuit breaker
    async fn make_request(&mut self, url: &str) -> Result<reqwest::Response> {
        // Check circuit breaker first
        if !self.circuit_breaker.can_execute() {
            return Err(anyhow::anyhow!("Circuit breaker is open, skipping request"));
        }

        let mut last_error = None;
        let mut server_error_count = 0;
        const MAX_SERVER_ERRORS: u32 = 5;

        for attempt in 0..self.max_retries {
            if attempt > 0 {
                let backoff_delay = self.calculate_backoff_delay(attempt);
                info!("Retrying request after {:?} delay (attempt {})", backoff_delay, attempt + 1);
                sleep(backoff_delay).await;
            }

            // Always respect rate limiting
            let request_delay = self.calculate_request_delay();
            if attempt == 0 {
                debug!("Waiting {:?} before request to respect rate limits", request_delay);
            }
            sleep(request_delay).await;

            match self.client.get(url).send().await {
                Ok(response) => {
                    match response.status().as_u16() {
                        200..=299 => {
                            debug!("Request successful: {}", url);
                            self.circuit_breaker.record_success();
                            return Ok(response);
                        }
                        429 => {
                            // Rate limited - be extra respectful
                            warn!("Rate limited by server (429), increasing backoff");
                            let extended_delay = self.calculate_backoff_delay(attempt + 2);
                            sleep(extended_delay).await;
                            last_error = Some(anyhow::anyhow!("Rate limited: 429"));
                        }
                        500..=599 => {
                            // Server error - these might be temporary
                            server_error_count += 1;
                            let status = response.status();
                            warn!("Server error {} (count: {}/{}), will retry", status, server_error_count, MAX_SERVER_ERRORS);

                            self.circuit_breaker.record_failure();

                            // For server errors, use longer backoff
                            let server_backoff = self.calculate_server_error_backoff(server_error_count);
                            if server_error_count < MAX_SERVER_ERRORS {
                                info!("Waiting {:?} before retry due to server error", server_backoff);
                                sleep(server_backoff).await;
                            }

                            last_error = Some(anyhow::anyhow!("Server error: {}", status));

                            // If we've hit too many server errors, give up
                            if server_error_count >= MAX_SERVER_ERRORS {
                                return Err(anyhow::anyhow!("Too many server errors ({}), giving up", server_error_count));
                            }
                        }
                        400..=499 => {
                            // Client error - don't retry these
                            let status = response.status();
                            return Err(anyhow::anyhow!("Client error ({}): {}", status.as_u16(), status.canonical_reason().unwrap_or("Unknown")));
                        }
                        _ => {
                            let status = response.status();
                            warn!("Unexpected status code: {}", status);
                            last_error = Some(anyhow::anyhow!("Unexpected status: {}", status));
                        }
                    }
                }
                Err(e) => {
                    warn!("Network error on attempt {}: {}", attempt + 1, e);
                    self.circuit_breaker.record_failure();
                    last_error = Some(anyhow::anyhow!("Network error: {}", e));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Max retries exceeded")))
    }

    /// Adapt batch size based on server response patterns
    fn adapt_batch_size(&mut self, success: bool, response_time: TokioDuration) {
        if !self.adaptive_batch_size {
            return;
        }

        match (success, response_time.as_millis()) {
            // If successful and fast, we can try increasing batch size
            (true, ms) if ms < 1000 && self.batch_size < self.max_batch_size => {
                self.batch_size = std::cmp::min(self.batch_size + 50, self.max_batch_size);
                debug!("Increased batch size to {} due to fast response", self.batch_size);
            }
            // If successful but slow, decrease batch size
            (true, ms) if ms > 5000 && self.batch_size > self.min_batch_size => {
                self.batch_size = std::cmp::max(self.batch_size - 100, self.min_batch_size);
                debug!("Decreased batch size to {} due to slow response", self.batch_size);
            }
            // If failed, definitely decrease batch size
            (false, _) if self.batch_size > self.min_batch_size => {
                self.batch_size = std::cmp::max(self.batch_size - 200, self.min_batch_size);
                warn!("Decreased batch size to {} due to failure", self.batch_size);
            }
            _ => {} // No change needed
        }
    }

    /// Fetch export data with respectful error handling and adaptive batching
    async fn fetch_export(&mut self, query: &ExportQuery) -> Result<Vec<LogEntry>> {
        let mut url = format!("{}/export", self.base_url);
        let mut params = Vec::new();

        // Use current (possibly adapted) batch size
        let count = query.count.unwrap_or(self.batch_size);
        params.push(format!("count={}", count));

        if let Some(after) = query.after {
            params.push(format!("after={}", after.format("%Y-%m-%dT%H:%M:%SZ")));
        }

        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        info!("Fetching export from: {} (batch size: {})", url, count);

        let start_time = Instant::now();
        let result = self.make_request(&url).await;
        let response_time = start_time.elapsed();

        match result {
            Ok(response) => {
                // Check if we're being asked to slow down via headers
                if let Some(retry_after) = response.headers().get("retry-after") {
                    if let Ok(retry_seconds) = retry_after.to_str()?.parse::<u64>() {
                        warn!("Server requested retry-after: {}s", retry_seconds);
                        sleep(TokioDuration::from_secs(retry_seconds)).await;
                    }
                }

                let text = response.text().await
                    .context("Failed to read response body")?;

                let entries: Vec<LogEntry> = text
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| serde_json::from_str(line))
                    .collect::<Result<Vec<_>, _>>()
                    .context("Failed to parse JSONL response")?;

                info!("Successfully fetched {} entries in {:?}", entries.len(), response_time);
                self.adapt_batch_size(true, response_time);
                Ok(entries)
            }
            Err(e) => {
                self.adapt_batch_size(false, response_time);
                Err(e.context("Failed to fetch export data"))
            }
        }
    }

    /// Process entries with progress reporting and error handling
    async fn process_entries(&self, entries: &[LogEntry]) -> Result<()> {
        let mut processed_in_batch = 0;
        let mut errors = 0;
        const ERROR_THRESHOLD: usize = 10; // Stop if too many errors

        for (i, entry) in entries.iter().enumerate() {
            match self.db.process_log_entry(entry) {
                Ok(()) => {
                    processed_in_batch += 1;
                    if processed_in_batch % 100 == 0 {
                        debug!("Processed {} entries in current batch", processed_in_batch);
                        // Removed db.get_job and db.update_job here to reduce DB writes
                    }
                }
                Err(e) => {
                    errors += 1;
                    warn!("Failed to process entry {}: {}", i, e);

                    if errors > ERROR_THRESHOLD {
                        return Err(anyhow::anyhow!(
                            "Too many processing errors ({}/{}), stopping batch",
                            errors, entries.len()
                        ));
                    }
                }
            }

            // Add small delay even during processing to be gentle on the database
            if processed_in_batch % 50 == 0 {
                sleep(TokioDuration::from_millis(10)).await;
            }
        }

        if errors > 0 {
            warn!("Completed batch with {} errors out of {} entries", errors, entries.len());
        }

        Ok(())
    }

    /// Enhanced single sync with better error recovery
    pub async fn run_single_sync(&mut self) -> Result<()> {
        info!("Starting respectful sync cycle");

        let mut job = ScraperJob::new("export_sync".to_string());
        job.start();
        self.db.insert_job(&job).context("Failed to create job")?;

        let stats = self.db.get_stats()?;
        let mut query = ExportQuery {
            count: Some(self.batch_size),
            after: stats.last_sync,
        };

        let mut total_processed_overall = 0;
        let mut consecutive_empty_batches = 0;
        const MAX_EMPTY_BATCHES: usize = 3;

        loop {
            // Check circuit breaker before each batch
            if !self.circuit_breaker.can_execute() {
                warn!("Circuit breaker open, aborting sync");
                job.fail("Circuit breaker open".to_string());
                self.db.update_job(&job)?;
                return Err(anyhow::anyhow!("Circuit breaker prevented sync completion"));
            }

            match self.fetch_export(&mut query).await {
                Ok(entries) => {
                    if entries.is_empty() {
                        consecutive_empty_batches += 1;
                        info!("Received empty batch ({}/{})", consecutive_empty_batches, MAX_EMPTY_BATCHES);

                        if consecutive_empty_batches >= MAX_EMPTY_BATCHES {
                            info!("Multiple empty batches received, sync appears complete");
                            break;
                        }

                        // Wait longer between empty batches to be respectful
                        sleep(TokioDuration::from_secs(10)).await;
                        continue;
                    }

                    consecutive_empty_batches = 0;
                    let batch_size = entries.len();

                    // Process the entries
                    if let Err(e) = self.process_entries(&entries).await {
                        error!("Failed to process batch: {}", e);
                        job.fail(e.to_string());
                        self.db.update_job(&job)?;
                        return Err(e);
                    }

                    total_processed_overall += batch_size;
                    info!("Processed {} entries in this batch. Total: {} (current batch size: {})",
                          batch_size, total_processed_overall, self.batch_size);

                    // Update cursor for next batch
                    if let Some(last_entry) = entries.last() {
                        query.after = Some(last_entry.created_at);
                        query.count = Some(self.batch_size); // Ensure this reflects the current adaptive batch size
                        job.update_progress(total_processed_overall as i64, Some(last_entry.created_at.to_rfc3339()));
                        self.db.update_job(&job)?; // Update job progress after each batch
                    }

                    // Flush database periodically
                    if total_processed_overall % 1000 == 0 {
                        self.db.flush().context("Failed to flush database")?;
                    }

                    // If we got fewer entries than requested, we might be done
                    if batch_size < self.batch_size {
                        info!("Received partial batch ({}), sync likely complete", batch_size);
                        break;
                    }

                    // Add extra delay between batches to be respectful
                    let batch_delay = TokioDuration::from_secs(5);
                    debug!("Waiting {:?} between batches", batch_delay);
                    sleep(batch_delay).await;
                }
                Err(e) => {
                    error!("Failed to fetch export: {}", e);
                    job.fail(format!("Failed to fetch export: {}", e));
                    self.db.update_job(&job)?;
                    return Err(e);
                }
            }
        }

        // Final database operations
        self.db.flush().context("Failed to flush database")?;
        self.db.update_last_sync().context("Failed to update last sync time")?;

        job.complete();
        job.records_processed = total_processed_overall as i64;
        self.db.update_job(&job)?;

        info!("Sync completed successfully. Total processed: {} (final batch size: {})",
              total_processed_overall, self.batch_size);
        Ok(())
    }

    /// Enhanced continuous sync with circuit breaker and health monitoring
    pub async fn run_continuous_sync(&mut self, base_interval_seconds: u64) -> ! {
        info!("Starting continuous sync with {}s base intervals", base_interval_seconds);

        let startup_delay = TokioDuration::from_secs(10);
        info!("Waiting {:?} before first sync to be respectful", startup_delay);
        sleep(startup_delay).await;

        let mut consecutive_failures = 0;
        const MAX_FAILURES_FOR_BACKOFF: u32 = 3;

        loop {
            let sync_start = std::time::Instant::now();

            // Check if circuit breaker allows operation
            if !self.circuit_breaker.can_execute() {
                warn!("Circuit breaker is open, skipping sync cycle");
                let circuit_wait = TokioDuration::from_secs(60); // Wait 1 minute when circuit is open
                sleep(circuit_wait).await;
                continue;
            }

            match self.run_single_sync().await {
                Ok(()) => {
                    let sync_duration = sync_start.elapsed();
                    info!("Sync completed in {:?}", sync_duration);
                    consecutive_failures = 0; // Reset failure counter on success

                    // Normal interval on success
                    let next_sync_delay = TokioDuration::from_secs(base_interval_seconds);
                    info!("Next sync in {:?} (batch size: {})", next_sync_delay, self.batch_size);
                    sleep(next_sync_delay).await;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    error!("Sync failed (attempt {}): {}", consecutive_failures, e);

                    // Log circuit breaker state
                    match self.circuit_breaker.get_state() {
                        CircuitState::Open => warn!("Circuit breaker is open"),
                        CircuitState::HalfOpen => info!("Circuit breaker is half-open"),
                        CircuitState::Closed => debug!("Circuit breaker is closed"),
                    }

                    // Adaptive backoff based on consecutive failures
                    let error_multiplier = if consecutive_failures <= MAX_FAILURES_FOR_BACKOFF {
                        2_u64.pow(consecutive_failures as u32)
                    } else {
                        16 // Cap at 16x the base interval
                    };

                    let error_delay = TokioDuration::from_secs(base_interval_seconds * error_multiplier);
                    warn!("Waiting {:?} after error before retry (failure #{}, multiplier: {}x)",
                          error_delay, consecutive_failures, error_multiplier);
                    sleep(error_delay).await;
                }
            }
        }
    }

    /// Check server health before attempting sync
    pub async fn check_server_health(&mut self) -> Result<bool> {
        let health_url = format!("{}/", self.base_url);

        info!("Checking server health at {}", health_url);

        match self.client.get(&health_url).timeout(std::time::Duration::from_secs(10)).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    info!("Server health check passed ({})", status);
                    self.circuit_breaker.record_success();
                    Ok(true)
                } else {
                    warn!("Server health check failed ({})", status);
                    self.circuit_breaker.record_failure();
                    Ok(false)
                }
            }
            Err(e) => {
                warn!("Server health check error: {}", e);
                self.circuit_breaker.record_failure();
                Ok(false)
            }
        }
    }

    /// Enhanced sync with health check
    pub async fn run_single_sync_with_health_check(&mut self) -> Result<()> {
        // Check server health first
        if !self.check_server_health().await? {
            return Err(anyhow::anyhow!("Server health check failed, skipping sync"));
        }

        self.run_single_sync().await
    }

    /// Get server status information for debugging
    pub async fn get_server_info(&mut self) -> Result<String> {
        let info_url = format!("{}/", self.base_url);

        match self.make_request(&info_url).await {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                let server_header = headers.get("server")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("unknown");

                Ok(format!("Status: {}, Server: {}", status, server_header))
            }
            Err(e) => {
                Err(anyhow::anyhow!("Failed to get server info: {}", e))
            }
        }
    }

    /// Check if we should be running (e.g., during business hours)
    pub fn should_run_now(&self) -> bool {
        let now = Utc::now();
        let hour = now.hour();

        matches!(hour, 2..=6 | 22..=23) 
    }

    /// Get configured rate limit info
    pub fn get_rate_limit_info(&self) -> (u32, TokioDuration) {
        (self.requests_per_minute, self.min_delay_between_requests)
    }

    /// Configure batch size (useful for adjusting load)
    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = std::cmp::max(
            std::cmp::min(batch_size, self.max_batch_size),
            self.min_batch_size
        );
        info!("Updated batch size to {}", self.batch_size);
    }

    /// Configure rate limiting
    pub fn set_rate_limit(&mut self, requests_per_minute: u32, min_delay_ms: u64) {
        self.requests_per_minute = requests_per_minute;
        self.min_delay_between_requests = TokioDuration::from_millis(min_delay_ms);
        info!("Updated rate limit to {} req/min with {}ms min delay", requests_per_minute, min_delay_ms);
    }

    /// Enable or disable adaptive batch sizing
    pub fn set_adaptive_batching(&mut self, enabled: bool) {
        self.adaptive_batch_size = enabled;
        info!("Adaptive batch sizing: {}", if enabled { "enabled" } else { "disabled" });
    }

    /// Configure adaptive batch size limits
    pub fn set_batch_size_limits(&mut self, min_size: usize, max_size: usize) {
        self.min_batch_size = min_size;
        self.max_batch_size = max_size;
        // Ensure current batch size is within new limits
        self.batch_size = std::cmp::max(
            std::cmp::min(self.batch_size, self.max_batch_size),
            self.min_batch_size
        );
        info!("Updated batch size limits: {}-{}, current: {}",
              self.min_batch_size, self.max_batch_size, self.batch_size);
    }

    /// Get scraper health status
    pub fn get_health_status(&self) -> serde_json::Value {
        serde_json::json!({
            "circuit_breaker_state": format!("{:?}", self.circuit_breaker.get_state()),
            "current_batch_size": self.batch_size,
            "adaptive_batching": self.adaptive_batch_size,
            "batch_size_limits": {
                "min": self.min_batch_size,
                "max": self.max_batch_size
            },
            "rate_limit": {
                "requests_per_minute": self.requests_per_minute,
                "min_delay_ms": self.min_delay_between_requests.as_millis()
            },
            "circuit_breaker": {
                "failure_count": self.circuit_breaker.failure_count,
                "failure_threshold": self.circuit_breaker.failure_threshold,
                "timeout_seconds": self.circuit_breaker.timeout.as_secs()
            }
        })
    }

    /// Reset circuit breaker (useful for manual recovery)
    pub fn reset_circuit_breaker(&mut self) {
        self.circuit_breaker.state = CircuitState::Closed;
        self.circuit_breaker.failure_count = 0;
        self.circuit_breaker.last_failure_time = None;
        info!("Circuit breaker manually reset");
    }

    /// Configure circuit breaker parameters
    pub fn set_circuit_breaker_config(&mut self, failure_threshold: u32, timeout_seconds: u64) {
        self.circuit_breaker.failure_threshold = failure_threshold;
        self.circuit_breaker.timeout = TokioDuration::from_secs(timeout_seconds);
        info!("Updated circuit breaker: {} failures threshold, {}s timeout",
              failure_threshold, timeout_seconds);
    }
}
