// src/main.rs

use askama::Template;

use crate::database::Database;
use crate::models::{DidRecord, OperationRecord, ScraperJob, Stats, SystemSettings, ScraperConfig};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::scraper::Scraper;

mod database;
mod models;
mod scraper;

// State and Template Structs

#[derive(Clone)]
struct AppState {
    db: Arc<Database>,
}

// Helper struct for DID display to simplify template logic
struct DidRecordForDisplay {
    did: String,
    handle: String,
    pds_endpoint: String,
    last_seen: chrono::DateTime<chrono::Utc>,
}

#[derive(Template)]
#[template(path = "layout.html")]
struct LayoutTemplate<T: Template> {
    title: String,
    content: T,
}

#[derive(Template)]
#[template(path = "partials/dashboard.html", escape = "html")]
struct DashboardPartial {
    stats: Stats,
}

#[derive(Template)]
#[template(path = "partials/scrapers.html")]
struct ControlPartial {
    config: ScraperConfig,
}

#[derive(Template)]
#[template(path = "partials/dids.html", escape = "html")]
struct DidsPartial {
    dids: Vec<DidRecordForDisplay>,
    search_query: Option<String>,
    current_page: usize,
    has_next_page: bool,
    total_found: usize,
}

#[derive(Template)]
#[template(path = "partials/history.html")]
struct HistoryPartial {
    jobs: Vec<ScraperJob>,
}


#[derive(Template)]
#[template(path = "partials/settings.html")]
struct SettingsPartial {
    settings: SystemSettings,
}

// === Query Parameters ===

#[derive(Deserialize)]
struct DidsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    page: usize,
    search: Option<String>,
}

#[derive(Deserialize)]
struct PaginationQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    50
}

// === Route Handlers ===

async fn show_dashboard(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let mut stats = state.db.get_stats().unwrap_or_default();

    let average_f64 = if stats.total_dids > 0 {
        stats.total_operations as f64 / stats.total_dids as f64
    } else {
        0.0
    };
    stats.average_ops_per_did = format!("{:.2}", average_f64);

    let partial = DashboardPartial { stats };

    if headers.contains_key("HX-Request") {
        Html(partial.render().unwrap()).into_response()
    } else {
        let layout = LayoutTemplate {
            title: "Dashboard".to_string(),
            content: partial,
        };
        Html(layout.render().unwrap()).into_response()
    }
}

async fn show_control(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let config = state.db.get_scraper_config().unwrap_or_default();
    let partial = ControlPartial { config };
    if headers.contains_key("HX-Request") {
        Html(partial.render().unwrap()).into_response()
    } else {
        let layout = LayoutTemplate {
            title: "Scraper Control".to_string(),
            content: partial,
        };
        Html(layout.render().unwrap()).into_response()
    }
}

async fn show_dids(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(query): Query<DidsQuery>,
) -> Response {
    let page = if query.page == 0 { 1 } else { query.page };
    let offset = (page - 1) * query.limit;
    
    let (did_records, total_found) = if let Some(ref search_term) = query.search {
        if search_term.trim().is_empty() {
            // Empty search, treat as no search
            let records = state
                .db
                .get_dids_paginated(query.limit, offset)
                .unwrap_or_default();
            let total = state.db.get_stats().map(|s| s.total_dids as usize).unwrap_or(0);
            (records, total)
        } else {
            // Perform search
            let records = state
                .db
                .search_dids(search_term.trim(), query.limit, offset)
                .unwrap_or_default();
            let total = state
                .db
                .count_search_results(search_term.trim())
                .unwrap_or(0);
            (records, total)
        }
    } else {
        // No search query
        let records = state
            .db
            .get_dids_paginated(query.limit, offset)
            .unwrap_or_default();
        let total = state.db.get_stats().map(|s| s.total_dids as usize).unwrap_or(0);
        (records, total)
    };

    let dids_for_display: Vec<DidRecordForDisplay> = did_records.into_iter().map(|record| {
        let handle = record.document_val
            .get("alsoKnownAs")
            .and_then(|aka| aka.as_array())
            .and_then(|arr| arr.get(0))
            .and_then(|v| v.as_str())
            .unwrap_or("N/A")
            .to_string();

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
            .unwrap_or("N/A")
            .to_string();

        DidRecordForDisplay {
            did: record.did,
            handle,
            pds_endpoint,
            last_seen: record.updated_at,
        }
    }).collect();

    let has_next_page = total_found > offset + query.limit;
    
    let partial = DidsPartial { 
        dids: dids_for_display,
        search_query: query.search.clone(),
        current_page: page,
        has_next_page,
        total_found,
    };

    if headers.contains_key("HX-Request") {
        Html(partial.render().unwrap()).into_response()
    } else {
        let layout = LayoutTemplate {
            title: "DID Directory".to_string(),
            content: partial,
        };
        Html(layout.render().unwrap()).into_response()
    }
}



async fn show_history(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let jobs = state.db.get_scraper_jobs().unwrap_or_default();
    let partial = HistoryPartial { jobs };
    if headers.contains_key("HX-Request") {
        Html(partial.render().unwrap()).into_response()
    } else {
        let layout = LayoutTemplate {
            title: "Job History".to_string(),
            content: partial,
        };
        Html(layout.render().unwrap()).into_response()
    }
}



async fn show_settings(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let settings = state.db.get_system_settings().unwrap_or_default();
    let partial = SettingsPartial { settings };
    if headers.contains_key("HX-Request") {
        Html(partial.render().unwrap()).into_response()
    } else {
        let layout = LayoutTemplate {
            title: "Settings".to_string(),
            content: partial,
        };
        Html(layout.render().unwrap()).into_response()
    }
}

// === API Handlers ===

#[axum::debug_handler]
async fn resolve_did(
    State(state): State<AppState>,
    Path(did): Path<String>,
) -> Result<Json<DidRecord>, StatusCode> {
    match state.db.get_did(&did) {
        Ok(Some(record)) if !record.is_tombstoned => Ok(Json(record)),
        Ok(Some(_)) => Err(StatusCode::GONE), // Tombstoned
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[axum::debug_handler]
async fn get_did_operations(
    State(state): State<AppState>,
    Path(did): Path<String>,
) -> Result<Json<Vec<OperationRecord>>, StatusCode> {
    match state.db.get_did_operations(&did) {
        Ok(operations) => Ok(Json(operations)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[axum::debug_handler]
async fn get_stats_api(State(state): State<AppState>) -> Result<Json<Stats>, StatusCode> {
    match state.db.get_stats() {
        Ok(mut stats) => {
            let average_f64 = if stats.total_dids > 0 {
                stats.total_operations as f64 / stats.total_dids as f64
            } else {
                0.0
            };
            stats.average_ops_per_did = format!("{:.2}", average_f64);
            Ok(Json(stats))
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[axum::debug_handler]
async fn list_dids_api(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationQuery>,
) -> Result<Json<Vec<DidRecord>>, StatusCode> {
    match state.db.get_dids_paginated(pagination.limit, pagination.offset) {
        Ok(dids) => Ok(Json(dids)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// === Main Application ===

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Initialize Database and State
    let db = Arc::new(Database::new("./plc_mirror.db").expect("Failed to open database"));
    let state = AppState { db: db.clone() };

    // Start scraper in background task
    let scraper_db = db.clone();
    tokio::spawn(async move {
        let mut scraper = Scraper::new(scraper_db);
        // Run once on startup, then sync every 5 minutes
        scraper.run_continuous_sync(300).await;
    });

    // Build Axum Router
    let app = Router::new()
        // HTML Frontend Routes
        .route("/", get(show_dashboard))
        .route("/control", get(show_control))
        .route("/dids", get(show_dids))
        .route("/history", get(show_history))
        .route("/settings", get(show_settings))
        // JSON API Routes (Mirroring plc.directory)
        .route("/{did}", get(resolve_did))
        .route("/{did}/log", get(get_did_operations))

        // Additional API endpoints
        .route("/api/stats", get(get_stats_api))
        .route("/api/dids", get(list_dids_api))
        .route("/api/{did}", get(resolve_did))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("🚀 PLC Directory Mirror running on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}
