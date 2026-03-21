use image::image_dimensions;
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};
use walkdir::{DirEntry, WalkDir};

const QUARANTINE_DIR_NAME: &str = ".picman-quarantine";
const SUPPORTED_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "heic"];
const MAX_WARNINGS: usize = 25;
const MAX_PREVIEW_GROUPS: usize = 12;
const SCAN_PROGRESS_EVENT: &str = "scan-progress";
const COUNT_PROGRESS_EMIT_INTERVAL: usize = 250;
const HASH_PROGRESS_EMIT_INTERVAL: usize = 25;
const SCAN_CANCELLED_MESSAGE: &str = "Analyse annulee.";

#[derive(Default)]
struct ScanControl {
    current_scan_cancel: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Clone, Debug)]
struct ScanRoot {
    canonical_path: PathBuf,
    display_name: String,
}

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
    root_paths: Vec<String>,
    quarantine_roots: Vec<String>,
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
    quarantine_paths: Vec<String>,
    moved_count: usize,
    failed: Vec<QuarantineFailure>,
}

#[derive(Clone, Debug)]
struct ScanCandidate {
    path: PathBuf,
    root_index: usize,
    display_path: String,
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
async fn scan_photo_library(app: AppHandle, root_paths: Vec<String>) -> Result<ScanReport, String> {
    let cancel_flag = register_scan_cancel_flag(&app)?;
    let app_handle = app.clone();
    let cancel_flag_clone = cancel_flag.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        scan_photo_library_impl(&app_handle, root_paths, &cancel_flag_clone)
    })
    .await
    .map_err(|error| format!("La tache d'analyse a echoue: {error}"))?;

    clear_scan_cancel_flag(&app, &cancel_flag);
    result
}

#[tauri::command]
fn cancel_scan(state: State<'_, ScanControl>) -> Result<bool, String> {
    let guard = state
        .current_scan_cancel
        .lock()
        .map_err(|_| "Le controle d'annulation du scan est indisponible.".to_string())?;

    if let Some(flag) = guard.as_ref() {
        flag.store(true, Ordering::SeqCst);
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
async fn quarantine_duplicates(
    root_paths: Vec<String>,
    paths: Vec<String>,
) -> Result<QuarantineResult, String> {
    tauri::async_runtime::spawn_blocking(move || quarantine_duplicates_impl(root_paths, paths))
        .await
        .map_err(|error| format!("La mise en quarantaine a echoue: {error}"))?
}

fn scan_photo_library_impl(
    app: &AppHandle,
    root_paths: Vec<String>,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<ScanReport, String> {
    let roots = canonicalize_existing_roots(root_paths)?;
    let use_root_prefix = roots.len() > 1;

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "counting",
            "Preparation de l'analyse des dossiers...",
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
    let inventory =
        collect_supported_photo_paths(&roots, use_root_prefix, app, cancel_flag, &mut warnings)?;
    let ScanInventory {
        hash_candidates,
        total_files_seen,
        supported_files,
        skipped_files,
        hash_candidate_files,
    } = inventory;

    if hash_candidate_files == 0 {
        return Ok(ScanReport {
            root_paths: roots
                .iter()
                .map(|root| path_to_string(&root.canonical_path))
                .collect(),
            quarantine_roots: roots
                .iter()
                .map(|root| path_to_string(&root.canonical_path.join(QUARANTINE_DIR_NAME)))
                .collect(),
            summary: ScanSummary {
                total_files_seen,
                supported_files,
                duplicate_groups: 0,
                duplicates_to_remove: 0,
                reclaimable_bytes: 0,
                skipped_files,
            },
            groups: Vec::new(),
            warnings,
        });
    }

    let hash_parallelism = recommended_hash_parallelism();
    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "hashing",
            format!(
                "{} photo(s) prises en charge, {} candidate(s) a hasher sur {} thread(s).",
                supported_files, hash_candidate_files, hash_parallelism
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

    let files_by_hash = Arc::new(Mutex::new(HashMap::<String, Vec<PhotoRecord>>::new()));
    let warning_buffer = Arc::new(Mutex::new(warnings));
    let skipped_counter = Arc::new(AtomicUsize::new(skipped_files));
    let processed_counter = Arc::new(AtomicUsize::new(0));
    let last_emitted = Arc::new(AtomicUsize::new(0));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(hash_parallelism)
        .build()
        .map_err(|error| format!("Impossible d'initialiser le pool de hash: {error}"))?;

    let hashing_result = pool.install(|| {
        hash_candidates.par_iter().try_for_each(|candidate| -> Result<(), String> {
            if is_scan_cancelled(cancel_flag) {
                return Err(SCAN_CANCELLED_MESSAGE.to_string());
            }

            let root = &roots[candidate.root_index];

            match build_photo_record(root, &candidate.path, use_root_prefix) {
                Ok((hash, record)) => {
                    let mut guard = files_by_hash.lock().map_err(|_| {
                        "Le regroupement des hash est devenu indisponible.".to_string()
                    })?;
                    guard.entry(hash).or_default().push(record);
                }
                Err(error) => {
                    skipped_counter.fetch_add(1, Ordering::SeqCst);
                    let mut warning_guard = warning_buffer.lock().map_err(|_| {
                        "Le buffer de warnings est devenu indisponible.".to_string()
                    })?;
                    push_warning(
                        &mut warning_guard,
                        format!("{}: {}", candidate.path.to_string_lossy(), error),
                    );
                }
            }

            let processed_items = processed_counter.fetch_add(1, Ordering::SeqCst) + 1;

            if should_emit_hash_progress(processed_items, hash_candidate_files)
                && last_emitted.swap(processed_items, Ordering::SeqCst) < processed_items
            {
                let preview_groups = {
                    let guard = files_by_hash.lock().map_err(|_| {
                        "Le regroupement des hash est devenu indisponible.".to_string()
                    })?;
                    build_duplicate_groups(&guard, Some(MAX_PREVIEW_GROUPS))
                };

                emit_scan_progress(
                    app,
                    ScanProgressPayload::new(
                        "hashing",
                        format!("Analyse des signatures {processed_items}/{hash_candidate_files}..."),
                        processed_items,
                        Some(hash_candidate_files),
                        Some(candidate.display_path.clone()),
                        total_files_seen,
                        supported_files,
                        hash_candidate_files,
                        preview_groups,
                    ),
                );
            }

            if is_scan_cancelled(cancel_flag) {
                return Err(SCAN_CANCELLED_MESSAGE.to_string());
            }

            Ok(())
        })
    });

    if let Err(error) = hashing_result {
        if error == SCAN_CANCELLED_MESSAGE {
            let preview_groups = {
                let guard = files_by_hash
                    .lock()
                    .map_err(|_| "Le regroupement des hash est devenu indisponible.".to_string())?;
                build_duplicate_groups(&guard, Some(MAX_PREVIEW_GROUPS))
            };

            emit_scan_progress(
                app,
                ScanProgressPayload::new(
                    "cancelled",
                    "Analyse annulee par l'utilisateur.",
                    processed_counter.load(Ordering::SeqCst),
                    Some(hash_candidate_files),
                    None,
                    total_files_seen,
                    supported_files,
                    hash_candidate_files,
                    preview_groups,
                ),
            );
        }

        return Err(error);
    }

    let warnings = Arc::try_unwrap(warning_buffer)
        .map_err(|_| "Impossible de finaliser les warnings du scan.".to_string())?
        .into_inner()
        .map_err(|_| "Le buffer de warnings est devenu indisponible.".to_string())?;

    let groups = {
        let guard = files_by_hash
            .lock()
            .map_err(|_| "Le regroupement des hash est devenu indisponible.".to_string())?;
        build_duplicate_groups(&guard, None)
    };

    let duplicate_groups = groups.len();
    let duplicates_to_remove = groups.iter().map(|group| group.file_count - 1).sum();
    let reclaimable_bytes = groups.iter().map(|group| group.reclaimable_bytes).sum();
    let skipped_files = skipped_counter.load(Ordering::SeqCst);

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
            build_duplicate_groups_preview(&groups),
        ),
    );

    Ok(ScanReport {
        root_paths: roots
            .iter()
            .map(|root| path_to_string(&root.canonical_path))
            .collect(),
        quarantine_roots: roots
            .iter()
            .map(|root| path_to_string(&root.canonical_path.join(QUARANTINE_DIR_NAME)))
            .collect(),
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
    root_paths: Vec<String>,
    paths: Vec<String>,
) -> Result<QuarantineResult, String> {
    if paths.is_empty() {
        return Err("Aucun doublon selectionne pour la quarantaine.".into());
    }

    let roots = canonicalize_existing_roots(root_paths)?;
    let mut moved_count = 0usize;
    let mut failed = Vec::new();
    let mut quarantine_directories: HashMap<usize, PathBuf> = HashMap::new();

    for path in paths {
        let source_path = PathBuf::from(&path);
        match find_matching_root_index(&roots, &source_path) {
            Ok(root_index) => {
                let batch_directory = quarantine_directories
                    .entry(root_index)
                    .or_insert_with(|| {
                        roots[root_index]
                            .canonical_path
                            .join(QUARANTINE_DIR_NAME)
                            .join(format!("batch-{}", unix_timestamp_seconds()))
                    })
                    .clone();

                if let Err(error) = create_quarantine_directory(&batch_directory).and_then(|_| {
                    move_file_to_quarantine(
                        &roots[root_index].canonical_path,
                        &batch_directory,
                        &source_path,
                    )
                }) {
                    failed.push(QuarantineFailure {
                        source_path: path,
                        reason: error,
                    });
                } else {
                    moved_count += 1;
                }
            }
            Err(error) => failed.push(QuarantineFailure {
                source_path: path,
                reason: error,
            }),
        }
    }

    let quarantine_paths = quarantine_directories
        .into_values()
        .map(|path| path_to_string(&path))
        .collect();

    Ok(QuarantineResult {
        quarantine_paths,
        moved_count,
        failed,
    })
}

fn collect_supported_photo_paths(
    roots: &[ScanRoot],
    use_root_prefix: bool,
    app: &AppHandle,
    cancel_flag: &Arc<AtomicBool>,
    warnings: &mut Vec<String>,
) -> Result<ScanInventory, String> {
    let mut size_groups: HashMap<u64, Vec<ScanCandidate>> = HashMap::new();
    let mut total_files_seen = 0usize;
    let mut supported_files = 0usize;
    let mut skipped_files = 0usize;

    for (root_index, root) in roots.iter().enumerate() {
        for entry in WalkDir::new(&root.canonical_path)
            .into_iter()
            .filter_entry(|entry| should_visit_entry(entry))
        {
            if is_scan_cancelled(cancel_flag) {
                return Err(SCAN_CANCELLED_MESSAGE.to_string());
            }

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
                            .push(ScanCandidate {
                                path: entry.path().to_path_buf(),
                                root_index,
                                display_path: display_path_for_root(
                                    root,
                                    entry.path(),
                                    use_root_prefix,
                                ),
                            });
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
                        Some(display_path_for_root(root, entry.path(), use_root_prefix)),
                        total_files_seen,
                        supported_files,
                        0,
                        Vec::new(),
                    ),
                );
            }
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

fn build_hash_candidates(size_groups: HashMap<u64, Vec<ScanCandidate>>) -> Vec<ScanCandidate> {
    let mut candidates = Vec::new();

    for (_, paths) in size_groups {
        if paths.len() > 1 {
            candidates.extend(paths);
        }
    }

    candidates.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    candidates
}

fn build_duplicate_groups(
    files_by_hash: &HashMap<String, Vec<PhotoRecord>>,
    limit: Option<usize>,
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

    if let Some(limit) = limit {
        groups.truncate(limit);
    }

    groups
}

fn build_duplicate_groups_preview(groups: &[DuplicateGroup]) -> Vec<DuplicateGroup> {
    groups.iter().take(MAX_PREVIEW_GROUPS).cloned().collect()
}

fn should_visit_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0 || entry.file_name() != QUARANTINE_DIR_NAME
}

fn canonicalize_existing_roots(root_paths: Vec<String>) -> Result<Vec<ScanRoot>, String> {
    let mut roots = Vec::new();
    let mut seen = HashMap::<String, ()>::new();

    for root_path in root_paths {
        let trimmed = root_path.trim();
        if trimmed.is_empty() {
            continue;
        }

        let canonical_path = canonicalize_existing_directory(trimmed)?;
        let key = canonical_path.to_string_lossy().to_ascii_lowercase();
        if seen.contains_key(&key) {
            continue;
        }
        seen.insert(key, ());

        roots.push(ScanRoot {
            display_name: canonical_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(trimmed)
                .to_string(),
            canonical_path,
        });
    }

    if roots.is_empty() {
        return Err("Ajoute au moins un dossier photo a analyser.".into());
    }

    Ok(roots)
}

fn canonicalize_existing_directory(path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(path);
    if !candidate.exists() {
        return Err(format!("Le dossier `{path}` n'existe pas."));
    }

    if !candidate.is_dir() {
        return Err(format!("Le chemin `{path}` doit pointer vers un dossier."));
    }

    candidate
        .canonicalize()
        .map_err(|error| format!("Impossible de resoudre le dossier `{path}`: {error}"))
}

fn find_matching_root_index(roots: &[ScanRoot], source_path: &Path) -> Result<usize, String> {
    let canonical_source = source_path
        .canonicalize()
        .map_err(|error| format!("chemin source invalide ({error})"))?;

    roots
        .iter()
        .enumerate()
        .filter(|(_, root)| canonical_source.starts_with(&root.canonical_path))
        .max_by_key(|(_, root)| root.canonical_path.components().count())
        .map(|(index, _)| index)
        .ok_or_else(|| "Le fichier n'appartient a aucun des dossiers scannes.".to_string())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .map(|extension| SUPPORTED_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

fn build_photo_record(
    root: &ScanRoot,
    path: &Path,
    use_root_prefix: bool,
) -> Result<(String, PhotoRecord), String> {
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
    let relative_path = display_path_for_root(root, &canonical_path, use_root_prefix);
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
    let mut buffer = [0u8; 128 * 1024];

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

fn create_quarantine_directory(batch_directory: &Path) -> Result<(), String> {
    fs::create_dir_all(batch_directory)
        .map_err(|error| format!("Impossible de creer la quarantaine: {error}"))
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
    processed_items == 1
        || processed_items == total_items
        || processed_items % HASH_PROGRESS_EMIT_INTERVAL == 0
}

fn display_path_for_root(root: &ScanRoot, path: &Path, use_root_prefix: bool) -> String {
    let relative = path_to_string(path.strip_prefix(&root.canonical_path).unwrap_or(path));
    if use_root_prefix {
        format!("{}\\{}", root.display_name, relative)
    } else {
        relative
    }
}

fn emit_scan_progress(app: &AppHandle, progress: ScanProgressPayload) {
    if let Err(error) = app.emit(SCAN_PROGRESS_EVENT, progress) {
        eprintln!("PicMan scan progress emit failed: {error}");
    }
}

fn recommended_hash_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(2)
        .clamp(1, 4)
}

fn is_scan_cancelled(cancel_flag: &Arc<AtomicBool>) -> bool {
    cancel_flag.load(Ordering::SeqCst)
}

fn register_scan_cancel_flag(app: &AppHandle) -> Result<Arc<AtomicBool>, String> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let state = app.state::<ScanControl>();
    let mut guard = state
        .current_scan_cancel
        .lock()
        .map_err(|_| "Le controle d'annulation du scan est indisponible.".to_string())?;
    *guard = Some(cancel_flag.clone());
    Ok(cancel_flag)
}

fn clear_scan_cancel_flag(app: &AppHandle, cancel_flag: &Arc<AtomicBool>) {
    if let Ok(mut guard) = app.state::<ScanControl>().current_scan_cancel.lock() {
        if let Some(current_flag) = guard.as_ref() {
            if Arc::ptr_eq(current_flag, cancel_flag) {
                *guard = None;
            }
        }
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
        .manage(ScanControl::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_photo_library,
            cancel_scan,
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
        size_groups.insert(
            100,
            vec![ScanCandidate {
                path: PathBuf::from("a.jpg"),
                root_index: 0,
                display_path: "a.jpg".into(),
            }],
        );
        size_groups.insert(
            200,
            vec![
                ScanCandidate {
                    path: PathBuf::from("b.jpg"),
                    root_index: 0,
                    display_path: "b.jpg".into(),
                },
                ScanCandidate {
                    path: PathBuf::from("c.jpg"),
                    root_index: 0,
                    display_path: "c.jpg".into(),
                },
            ],
        );

        let candidates = build_hash_candidates(size_groups);
        let paths: Vec<String> = candidates
            .into_iter()
            .map(|candidate| candidate.path.to_string_lossy().to_string())
            .collect();

        assert_eq!(paths, vec!["b.jpg".to_string(), "c.jpg".to_string()]);
    }
}
