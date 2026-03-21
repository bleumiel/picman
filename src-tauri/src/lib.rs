use image::image_dimensions;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use walkdir::{DirEntry, WalkDir};

const QUARANTINE_DIR_NAME: &str = ".picman-quarantine";
const SUPPORTED_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "heic"];
const MAX_WARNINGS: usize = 25;
const SCAN_PROGRESS_EVENT: &str = "scan-progress";
const COUNT_PROGRESS_EMIT_INTERVAL: usize = 250;
const HASH_PROGRESS_EMIT_INTERVAL: usize = 10;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhotoRecord {
    path: String,
    relative_path: String,
    file_name: String,
    extension: String,
    size_bytes: u64,
    width: Option<u32>,
    height: Option<u32>,
    modified_unix_ms: Option<u64>,
    quality_score: u64,
    quality_reason: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DuplicateGroup {
    hash: String,
    file_count: usize,
    total_size_bytes: u64,
    reclaimable_bytes: u64,
    keep_path: String,
    keep_relative_path: String,
    keep_reason: String,
    files: Vec<PhotoRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanSummary {
    total_files_seen: usize,
    supported_files: usize,
    duplicate_groups: usize,
    duplicates_to_remove: usize,
    reclaimable_bytes: u64,
    skipped_files: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanReport {
    root_path: String,
    quarantine_root: String,
    summary: ScanSummary,
    groups: Vec<DuplicateGroup>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgressPayload {
    phase: String,
    message: String,
    processed_items: usize,
    total_items: Option<usize>,
    current_path: Option<String>,
    total_files_seen: usize,
    supported_files: usize,
    hash_candidate_files: usize,
    preview_groups: Vec<DuplicateGroup>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineFailure {
    source_path: String,
    reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineResult {
    quarantine_path: String,
    moved_count: usize,
    failed: Vec<QuarantineFailure>,
}

#[derive(Clone, Debug)]
struct ScanCandidate {
    path: PathBuf,
}

#[derive(Debug)]
struct ScanInventory {
    hash_candidates: Vec<ScanCandidate>,
    total_files_seen: usize,
    supported_files: usize,
    skipped_files: usize,
    hash_candidate_files: usize,
}

#[tauri::command]
async fn scan_photo_library(app: AppHandle, root_path: String) -> Result<ScanReport, String> {
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || scan_photo_library_impl(&app_handle, &root_path))
        .await
        .map_err(|error| format!("La tache d'analyse a echoue: {error}"))?
}

#[tauri::command]
async fn quarantine_duplicates(
    root_path: String,
    paths: Vec<String>,
) -> Result<QuarantineResult, String> {
    tauri::async_runtime::spawn_blocking(move || quarantine_duplicates_impl(&root_path, paths))
        .await
        .map_err(|error| format!("La mise en quarantaine a echoue: {error}"))?
}

fn scan_photo_library_impl(app: &AppHandle, root_path: &str) -> Result<ScanReport, String> {
    let root = canonicalize_existing_directory(root_path)?;
    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "counting",
            "Preparation de l'analyse du dossier...",
            0,
            None,
            None,
            0,
            0,
            0,
            Vec::new(),
        ),
    );

    let mut warnings = Vec::new();
    let inventory = collect_supported_photo_paths(&root, app, &mut warnings)?;
    let ScanInventory {
        hash_candidates,
        total_files_seen,
        supported_files,
        skipped_files,
        hash_candidate_files,
    } = inventory;

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "hashing",
            format!(
                "{} photo(s) prises en charge, {} candidate(s) a hasher apres filtrage par taille.",
                supported_files, hash_candidate_files
            ),
            0,
            Some(hash_candidate_files),
            None,
            total_files_seen,
            supported_files,
            hash_candidate_files,
            Vec::new(),
        ),
    );

    let mut skipped_files = skipped_files;
    let mut files_by_hash: HashMap<String, Vec<PhotoRecord>> = HashMap::new();

    for (index, candidate) in hash_candidates.iter().enumerate() {
        let current_path = Some(relative_path_from_root(&root, &candidate.path));

        match build_photo_record(&root, &candidate.path) {
            Ok((hash, record)) => files_by_hash.entry(hash).or_default().push(record),
            Err(error) => {
                skipped_files += 1;
                push_warning(
                    &mut warnings,
                    format!("{}: {}", candidate.path.to_string_lossy(), error),
                );
            }
        }

        let processed_items = index + 1;
        if should_emit_hash_progress(processed_items, hash_candidate_files) {
            let preview_groups = build_duplicate_groups(&files_by_hash);
            emit_scan_progress(
                app,
                ScanProgressPayload::new(
                    "hashing",
                    format!("Analyse des signatures {processed_items}/{hash_candidate_files}..."),
                    processed_items,
                    Some(hash_candidate_files),
                    current_path,
                    total_files_seen,
                    supported_files,
                    hash_candidate_files,
                    preview_groups,
                ),
            );
        }
    }

    let groups = build_duplicate_groups(&files_by_hash);
    let duplicate_groups = groups.len();
    let duplicates_to_remove = groups.iter().map(|group| group.file_count - 1).sum();
    let reclaimable_bytes = groups.iter().map(|group| group.reclaimable_bytes).sum();

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "grouping",
            "Regroupement des doublons exacts...",
            hash_candidate_files,
            Some(hash_candidate_files),
            None,
            total_files_seen,
            supported_files,
            hash_candidate_files,
            groups.clone(),
        ),
    );

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "complete",
            format!("Analyse terminee: {duplicate_groups} groupe(s) de doublons detecte(s)."),
            hash_candidate_files,
            Some(hash_candidate_files),
            None,
            total_files_seen,
            supported_files,
            hash_candidate_files,
            groups.clone(),
        ),
    );

    Ok(ScanReport {
        root_path: path_to_string(&root),
        quarantine_root: path_to_string(&root.join(QUARANTINE_DIR_NAME)),
        summary: ScanSummary {
            total_files_seen,
            supported_files,
            duplicate_groups,
            duplicates_to_remove,
            reclaimable_bytes,
            skipped_files,
        },
        groups,
        warnings,
    })
}

fn quarantine_duplicates_impl(
    root_path: &str,
    paths: Vec<String>,
) -> Result<QuarantineResult, String> {
    if paths.is_empty() {
        return Err("Aucun doublon selectionne pour la quarantaine.".into());
    }

    let root = canonicalize_existing_directory(root_path)?;
    let quarantine_root = root.join(QUARANTINE_DIR_NAME);
    let batch_directory = quarantine_root.join(format!("batch-{}", unix_timestamp_seconds()));
    fs::create_dir_all(&batch_directory)
        .map_err(|error| format!("Impossible de creer la quarantaine: {error}"))?;

    let mut moved_count = 0usize;
    let mut failed = Vec::new();

    for path in paths {
        if let Err(error) = move_file_to_quarantine(&root, &batch_directory, Path::new(&path)) {
            failed.push(QuarantineFailure {
                source_path: path,
                reason: error,
            });
        } else {
            moved_count += 1;
        }
    }

    Ok(QuarantineResult {
        quarantine_path: path_to_string(&batch_directory),
        moved_count,
        failed,
    })
}

fn collect_supported_photo_paths(
    root: &Path,
    app: &AppHandle,
    warnings: &mut Vec<String>,
) -> Result<ScanInventory, String> {
    let mut size_groups: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut total_files_seen = 0usize;
    let mut supported_files = 0usize;
    let mut skipped_files = 0usize;

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| should_visit_entry(entry))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                skipped_files += 1;
                push_warning(warnings, format!("Entree ignoree: {error}"));
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        total_files_seen += 1;

        if is_supported_image(entry.path()) {
            match entry.metadata() {
                Ok(metadata) => {
                    supported_files += 1;
                    size_groups
                        .entry(metadata.len())
                        .or_default()
                        .push(entry.path().to_path_buf());
                }
                Err(error) => {
                    skipped_files += 1;
                    push_warning(
                        warnings,
                        format!(
                            "{}: metadata indisponibles ({error})",
                            entry.path().to_string_lossy()
                        ),
                    );
                }
            }
        } else {
            skipped_files += 1;
        }

        if should_emit_count_progress(total_files_seen) {
            emit_scan_progress(
                app,
                ScanProgressPayload::new(
                    "counting",
                    format!(
                        "{} entree(s) parcourue(s), {} photo(s) retenue(s).",
                        total_files_seen, supported_files
                    ),
                    total_files_seen,
                    None,
                    Some(relative_path_from_root(root, entry.path())),
                    total_files_seen,
                    supported_files,
                    0,
                    Vec::new(),
                ),
            );
        }
    }

    let hash_candidates = build_hash_candidates(size_groups);
    let hash_candidate_files = hash_candidates.len();

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "counting",
            format!(
                "Preparation terminee: {} photo(s) en file d'attente, {} candidate(s) a hasher.",
                supported_files, hash_candidate_files
            ),
            total_files_seen,
            None,
            None,
            total_files_seen,
            supported_files,
            hash_candidate_files,
            Vec::new(),
        ),
    );

    Ok(ScanInventory {
        hash_candidates,
        total_files_seen,
        supported_files,
        skipped_files,
        hash_candidate_files,
    })
}

fn build_hash_candidates(size_groups: HashMap<u64, Vec<PathBuf>>) -> Vec<ScanCandidate> {
    let mut candidates = Vec::new();

    for (_, paths) in size_groups {
        if paths.len() > 1 {
            for path in paths {
                candidates.push(ScanCandidate { path });
            }
        }
    }

    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    candidates
}

fn build_duplicate_groups(
    files_by_hash: &HashMap<String, Vec<PhotoRecord>>,
) -> Vec<DuplicateGroup> {
    let mut groups: Vec<DuplicateGroup> = files_by_hash
        .iter()
        .filter_map(|(hash, files)| {
            if files.len() < 2 {
                return None;
            }

            let mut files = files.clone();
            files.sort_by(compare_keep_candidates);
            let keep_file = files.first()?.clone();
            let total_size_bytes = files.iter().map(|file| file.size_bytes).sum();
            let reclaimable_bytes = files.iter().skip(1).map(|file| file.size_bytes).sum();
            let keep_reason = explain_keep_choice(&keep_file, &files);

            Some(DuplicateGroup {
                hash: hash.clone(),
                file_count: files.len(),
                total_size_bytes,
                reclaimable_bytes,
                keep_path: keep_file.path.clone(),
                keep_relative_path: keep_file.relative_path.clone(),
                keep_reason,
                files,
            })
        })
        .collect();

    groups.sort_by(|left, right| {
        right
            .reclaimable_bytes
            .cmp(&left.reclaimable_bytes)
            .then_with(|| right.file_count.cmp(&left.file_count))
            .then_with(|| left.keep_relative_path.cmp(&right.keep_relative_path))
    });

    groups
}

fn should_visit_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0 || entry.file_name() != QUARANTINE_DIR_NAME
}

fn canonicalize_existing_directory(path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(path);
    if !candidate.exists() {
        return Err("Le dossier a analyser n'existe pas.".into());
    }

    if !candidate.is_dir() {
        return Err("Le chemin fourni doit pointer vers un dossier.".into());
    }

    candidate
        .canonicalize()
        .map_err(|error| format!("Impossible de resoudre le dossier: {error}"))
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .map(|extension| SUPPORTED_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

fn build_photo_record(root: &Path, path: &Path) -> Result<(String, PhotoRecord), String> {
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("chemin non resolu ({error})"))?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|error| format!("metadata indisponibles ({error})"))?;
    let extension = canonical_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| "extension manquante".to_string())?;

    let dimensions = read_dimensions(&canonical_path, &extension);
    let size_bytes = metadata.len();
    let quality_score = compute_quality_score(&extension, dimensions, size_bytes);
    let quality_reason = build_quality_reason(&extension, dimensions, size_bytes);
    let relative_path = relative_path_from_root(root, &canonical_path);
    let hash = compute_sha256(&canonical_path)?;

    Ok((
        hash,
        PhotoRecord {
            path: path_to_string(&canonical_path),
            relative_path,
            file_name: canonical_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string(),
            extension,
            size_bytes,
            width: dimensions.map(|(width, _)| width),
            height: dimensions.map(|(_, height)| height),
            modified_unix_ms: metadata
                .modified()
                .ok()
                .and_then(|timestamp| timestamp.duration_since(UNIX_EPOCH).ok())
                .and_then(|duration| u64::try_from(duration.as_millis()).ok()),
            quality_score,
            quality_reason,
        },
    ))
}

fn read_dimensions(path: &Path, extension: &str) -> Option<(u32, u32)> {
    match extension {
        "jpg" | "jpeg" | "png" => image_dimensions(path).ok(),
        _ => None,
    }
}

fn compute_sha256(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|error| format!("ouverture impossible ({error})"))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8 * 1024];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| format!("lecture impossible ({error})"))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn compute_quality_score(extension: &str, dimensions: Option<(u32, u32)>, size_bytes: u64) -> u64 {
    let pixel_score = dimensions
        .map(|(width, height)| u64::from(width) * u64::from(height))
        .unwrap_or(0);
    let format_bonus = match extension {
        "png" => 3_000_000_000,
        "heic" => 2_000_000_000,
        "jpg" | "jpeg" => 1_000_000_000,
        _ => 0,
    };

    pixel_score
        .saturating_mul(10)
        .saturating_add(size_bytes / 1024)
        .saturating_add(format_bonus)
}

fn build_quality_reason(
    extension: &str,
    dimensions: Option<(u32, u32)>,
    size_bytes: u64,
) -> String {
    match dimensions {
        Some((width, height)) => format!(
            "{}x{}, format {}, {} octets",
            width,
            height,
            extension.to_uppercase(),
            size_bytes
        ),
        None => format!(
            "dimensions non lues, format {}, {} octets",
            extension.to_uppercase(),
            size_bytes
        ),
    }
}

fn compare_keep_candidates(left: &PhotoRecord, right: &PhotoRecord) -> std::cmp::Ordering {
    right
        .quality_score
        .cmp(&left.quality_score)
        .then_with(|| path_depth(&left.relative_path).cmp(&path_depth(&right.relative_path)))
        .then_with(|| left.relative_path.len().cmp(&right.relative_path.len()))
        .then_with(|| {
            left.relative_path
                .to_ascii_lowercase()
                .cmp(&right.relative_path.to_ascii_lowercase())
        })
}

fn explain_keep_choice(keep_file: &PhotoRecord, files: &[PhotoRecord]) -> String {
    let same_score_count = files
        .iter()
        .filter(|file| file.quality_score == keep_file.quality_score)
        .count();

    if same_score_count == files.len() {
        format!(
            "Copies exactes detectees. Toutes les versions ont le meme score; PicMan conserve `{}` car son chemin est le plus court et le plus stable.",
            keep_file.relative_path
        )
    } else {
        format!(
            "Conservee car elle obtient le meilleur score ({}) sur ce groupe: {}.",
            keep_file.quality_score, keep_file.quality_reason
        )
    }
}

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn move_file_to_quarantine(
    root: &Path,
    batch_directory: &Path,
    source_path: &Path,
) -> Result<(), String> {
    let canonical_source = source_path
        .canonicalize()
        .map_err(|error| format!("chemin source invalide ({error})"))?;

    if !canonical_source.starts_with(root) {
        return Err("Le fichier n'appartient pas au dossier scanne.".into());
    }

    let relative_path = canonical_source
        .strip_prefix(root)
        .map_err(|error| format!("chemin relatif introuvable ({error})"))?;

    let mut components = relative_path.components();
    if components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        == Some(QUARANTINE_DIR_NAME)
    {
        return Err("Le fichier est deja dans la quarantaine.".into());
    }

    let target_path = next_available_target(&batch_directory.join(relative_path));
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("creation du dossier cible impossible ({error})"))?;
    }

    fs::rename(&canonical_source, &target_path)
        .map_err(|error| format!("deplacement impossible ({error})"))
}

fn next_available_target(initial_target: &Path) -> PathBuf {
    if !initial_target.exists() {
        return initial_target.to_path_buf();
    }

    let parent = initial_target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let stem = initial_target
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let extension = initial_target.extension().and_then(|value| value.to_str());

    for index in 1.. {
        let candidate_name = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("numeric suffixes should always produce a unique path")
}

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < MAX_WARNINGS {
        warnings.push(warning);
    }
}

fn should_emit_count_progress(total_files_seen: usize) -> bool {
    total_files_seen == 1 || total_files_seen % COUNT_PROGRESS_EMIT_INTERVAL == 0
}

fn should_emit_hash_progress(processed_items: usize, total_items: usize) -> bool {
    total_items == 0
        || processed_items == 1
        || processed_items == total_items
        || processed_items % HASH_PROGRESS_EMIT_INTERVAL == 0
}

fn relative_path_from_root(root: &Path, path: &Path) -> String {
    path_to_string(path.strip_prefix(root).unwrap_or(path))
}

fn emit_scan_progress(app: &AppHandle, progress: ScanProgressPayload) {
    if let Err(error) = app.emit(SCAN_PROGRESS_EVENT, progress) {
        eprintln!("PicMan scan progress emit failed: {error}");
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

impl ScanProgressPayload {
    fn new(
        phase: &str,
        message: impl Into<String>,
        processed_items: usize,
        total_items: Option<usize>,
        current_path: Option<String>,
        total_files_seen: usize,
        supported_files: usize,
        hash_candidate_files: usize,
        preview_groups: Vec<DuplicateGroup>,
    ) -> Self {
        Self {
            phase: phase.to_string(),
            message: message.into(),
            processed_items,
            total_items,
            current_path,
            total_files_seen,
            supported_files,
            hash_candidate_files,
            preview_groups,
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_photo_library,
            quarantine_duplicates
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_are_case_insensitive() {
        assert!(is_supported_image(Path::new(r"C:\Photos\summer.JPG")));
        assert!(is_supported_image(Path::new(r"C:\Photos\portrait.heic")));
        assert!(!is_supported_image(Path::new(r"C:\Photos\clip.mp4")));
    }

    #[test]
    fn keep_sort_prefers_higher_quality_before_shorter_path() {
        let stronger = PhotoRecord {
            path: String::new(),
            relative_path: r"album\best.jpg".into(),
            file_name: "best.jpg".into(),
            extension: "jpg".into(),
            size_bytes: 10,
            width: Some(4000),
            height: Some(3000),
            modified_unix_ms: None,
            quality_score: compute_quality_score("jpg", Some((4000, 3000)), 10),
            quality_reason: String::new(),
        };
        let weaker = PhotoRecord {
            path: String::new(),
            relative_path: r"album\deep\copy.jpg".into(),
            file_name: "copy.jpg".into(),
            extension: "jpg".into(),
            size_bytes: 10,
            width: Some(2000),
            height: Some(1000),
            modified_unix_ms: None,
            quality_score: compute_quality_score("jpg", Some((2000, 1000)), 10),
            quality_reason: String::new(),
        };

        let mut files = vec![weaker.clone(), stronger.clone()];
        files.sort_by(compare_keep_candidates);

        assert_eq!(files[0].relative_path, stronger.relative_path);
    }

    #[test]
    fn next_available_target_adds_numeric_suffix() {
        let temp_root =
            std::env::temp_dir().join(format!("picman-test-{}", unix_timestamp_seconds()));
        fs::create_dir_all(&temp_root).expect("temp directory should be created");

        let taken = temp_root.join("photo.jpg");
        fs::write(&taken, b"taken").expect("temp file should be written");

        let candidate = next_available_target(&taken);
        assert_eq!(
            candidate.file_name().and_then(|value| value.to_str()),
            Some("photo-1.jpg")
        );

        fs::remove_dir_all(&temp_root).expect("temp directory should be removed");
    }

    #[test]
    fn build_hash_candidates_only_keeps_size_collisions() {
        let mut size_groups = HashMap::new();
        size_groups.insert(100, vec![PathBuf::from("a.jpg")]);
        size_groups.insert(200, vec![PathBuf::from("b.jpg"), PathBuf::from("c.jpg")]);

        let candidates = build_hash_candidates(size_groups);
        let paths: Vec<String> = candidates
            .into_iter()
            .map(|candidate| candidate.path.to_string_lossy().to_string())
            .collect();

        assert_eq!(paths, vec!["b.jpg".to_string(), "c.jpg".to_string()]);
    }
}
