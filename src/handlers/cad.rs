use axum::{
    body::Body,
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use tokio_util::io::ReaderStream;
use tracing::error;

const REPOS: &[(&str, &str)] = &[
    ("laminarforge-cad", "/Users/jarvisgpt/projects/laminarforge/laminarforge-cad"),
    ("gevdynamics-cad", "/Users/jarvisgpt/projects/gev-dynamics/gevdynamics-cad"),
];

fn resolve_output_dir(repo: &str) -> Option<PathBuf> {
    REPOS.iter()
        .find(|(name, _)| *name == repo)
        .map(|(_, path)| PathBuf::from(path).join("output"))
}

/// Derive a human-readable part name from a filename stem
/// e.g. "turbogen_compressor_wheel" → "Compressor Wheel"
fn humanize_part_name(stem: &str) -> String {
    let name = stem
        .strip_prefix("turbogen_")
        .or_else(|| stem.strip_prefix("lamp_"))
        .unwrap_or(stem);
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "usdz" => "model/vnd.usdz+zip",
        "stp" | "step" => "model/step",
        "stl" => "model/stl",
        "obj" => "model/obj",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Serialize)]
pub struct CadFileEntry {
    pub filename: String,
    pub part_name: String,
    pub repo: String,
    pub category: String,
    pub format: String,
    pub size_bytes: u64,
    pub last_modified: i64,
    pub usdz_available: bool,
    pub thumbnail_available: bool,
}

/// Derive a category from repo + filename for better grouping
fn categorize_file(repo: &str, stem: &str) -> &'static str {
    match repo {
        "gevdynamics-cad" => "Turbogenerator",
        "laminarforge-cad" => {
            if stem.starts_with("microfluidic_chip") || stem.starts_with("monolithic_board") {
                "Microfluidic Chips"
            } else if stem.starts_with("syringe_pump") || stem.starts_with("syringe_motor")
                || stem.starts_with("syringe_plunger") || stem.starts_with("syringe_clamp")
            {
                "Syringe Pump"
            } else if stem.starts_with("heating_block") || stem.starts_with("optical_mount")
                || stem.starts_with("lid") || stem.starts_with("enclosure")
                || stem.starts_with("tube_holder") || stem.starts_with("tube_fit")
            {
                "LAMP Device"
            } else if stem.starts_with("dispensing") || stem.starts_with("gantry")
                || stem.starts_with("xy_carriage") || stem.starts_with("z_carriage")
                || stem.starts_with("wash_station") || stem.starts_with("chip_adapter")
            {
                "Dispensing System"
            } else if stem.starts_with("co2_incubator") {
                "CO2 Incubator"
            } else if stem.starts_with("still_air_box") || stem.starts_with("workstation") {
                "Workstation"
            } else if stem.starts_with("water_bath") {
                "Water Bath"
            } else if stem.starts_with("swab") || stem.starts_with("microlattice") {
                "Sample Collection"
            } else {
                "LAMP Device"
            }
        }
        _ => "Other",
    }
}

#[derive(Debug, Deserialize)]
pub struct ListCadFilesQuery {
    pub repo: Option<String>,
}

/// GET /api/cad/files — list available CAD files with metadata
pub async fn list_cad_files(
    Query(query): Query<ListCadFilesQuery>,
) -> Response {
    let repos_to_scan: Vec<(&str, &str)> = if let Some(ref repo_filter) = query.repo {
        REPOS.iter()
            .filter(|(name, _)| name == repo_filter)
            .copied()
            .collect()
    } else {
        REPOS.to_vec()
    };

    if repos_to_scan.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Unknown repo" }))).into_response();
    }

    let mut files: Vec<CadFileEntry> = Vec::new();

    for (repo_name, repo_path) in &repos_to_scan {
        let output_dir = PathBuf::from(repo_path).join("output");
        let entries = match fs::read_dir(&output_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Collect all filenames first so we can check for .usdz/.thumb.png pairs
        let mut all_filenames: Vec<String> = Vec::new();
        let mut entry_data: Vec<(String, fs::Metadata)> = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            all_filenames.push(filename.clone());
            if let Ok(meta) = entry.metadata() {
                entry_data.push((filename, meta));
            }
        }

        // Build a set of stems that have .usdz files — only these are viewable
        let usdz_stems: std::collections::HashSet<String> = all_filenames.iter()
            .filter(|f| f.ends_with(".usdz"))
            .map(|f| f.strip_suffix(".usdz").unwrap_or(f).to_string())
            .collect();

        for stem in &usdz_stems {
            let usdz_filename = format!("{}.usdz", stem);

            // Prefer STEP metadata if available, else use USDZ file
            let (display_filename, meta) = if let Some((_, m)) = entry_data.iter().find(|(f, _)| f == &format!("{}.stp", stem)) {
                (format!("{}.stp", stem), m)
            } else if let Some((_, m)) = entry_data.iter().find(|(f, _)| f == &usdz_filename) {
                (usdz_filename.clone(), m)
            } else {
                continue;
            };

            let thumbnail_available = all_filenames.contains(&format!("{}.thumb.png", stem));

            let last_modified = meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            // Use USDZ file size for download estimation
            let usdz_size = entry_data.iter()
                .find(|(f, _)| f == &usdz_filename)
                .map(|(_, m)| m.len())
                .unwrap_or(meta.len());

            files.push(CadFileEntry {
                filename: display_filename,
                part_name: humanize_part_name(stem),
                repo: repo_name.to_string(),
                category: categorize_file(repo_name, stem).to_string(),
                format: "usdz".to_string(),
                size_bytes: usdz_size,
                last_modified,
                usdz_available: true,
                thumbnail_available,
            });
        }
    }

    files.sort_by(|a, b| a.category.cmp(&b.category).then(a.part_name.cmp(&b.part_name)));

    (StatusCode::OK, Json(json!({
        "files": files,
        "count": files.len(),
    }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct DownloadCadFileQuery {
    pub repo: Option<String>,
}

/// GET /api/cad/files/:filename — stream CAD file download
/// Supports USDZ, STEP, STL formats. Pass ?repo= to disambiguate.
pub async fn download_cad_file(
    Path(filename): Path<String>,
    Query(query): Query<DownloadCadFileQuery>,
) -> Response {
    // Validate filename — no path traversal
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid filename" }))).into_response();
    }

    // Find the file across repos (or in specific repo)
    let repos_to_check: Vec<(&str, &str)> = if let Some(ref repo) = query.repo {
        REPOS.iter().filter(|(name, _)| name == repo).copied().collect()
    } else {
        REPOS.to_vec()
    };

    let mut file_path: Option<PathBuf> = None;
    for (_, repo_path) in &repos_to_check {
        let candidate = PathBuf::from(repo_path).join("output").join(&filename);
        if candidate.exists() && candidate.is_file() {
            file_path = Some(candidate);
            break;
        }
    }

    let file_path = match file_path {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "File not found" }))).into_response(),
    };

    let ext = file_path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let content_type = mime_for_ext(&ext);

    let file_size = match fs::metadata(&file_path) {
        Ok(m) => m.len(),
        Err(e) => {
            error!("Failed to read file metadata: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Failed to read file" }))).into_response();
        }
    };

    // Stream the file using tokio
    let file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to open file {}: {:?}", filename, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Failed to open file" }))).into_response();
        }
    };

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_LENGTH, file_size.to_string()),
            (header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", filename)),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    ).into_response()
}

/// GET /api/cad/files/:filename/thumbnail — serve cached thumbnail PNG
pub async fn get_cad_thumbnail(
    Path(filename): Path<String>,
    Query(query): Query<DownloadCadFileQuery>,
) -> Response {
    // Validate filename
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid filename" }))).into_response();
    }

    // Derive thumbnail filename from the STEP/USDZ filename
    let stem = std::path::Path::new(&filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&filename);
    let thumb_filename = format!("{}.thumb.png", stem);

    let repos_to_check: Vec<(&str, &str)> = if let Some(ref repo) = query.repo {
        REPOS.iter().filter(|(name, _)| name == repo).copied().collect()
    } else {
        REPOS.to_vec()
    };

    let mut thumb_path: Option<PathBuf> = None;
    for (_, repo_path) in &repos_to_check {
        let candidate = PathBuf::from(repo_path).join("output").join(&thumb_filename);
        if candidate.exists() {
            thumb_path = Some(candidate);
            break;
        }
    }

    let thumb_path = match thumb_path {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "Thumbnail not available" }))).into_response(),
    };

    let content = match fs::read(&thumb_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read thumbnail: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Failed to read thumbnail" }))).into_response();
        }
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CONTENT_LENGTH, content.len().to_string()),
            (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
        ],
        content,
    ).into_response()
}
