use image::{image_dimensions, imageops::FilterType, ImageReader};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};
use walkdir::{DirEntry, WalkDir};

const QUARANTINE_DIR_NAME: &str = ".picman-quarantine";
const IMAGE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "heic"];
const MAX_WARNINGS: usize = 25;
const MAX_PREVIEW_GROUPS: usize = 12;
const SCAN_PROGRESS_EVENT: &str = "scan-progress";
const COUNT_PROGRESS_EMIT_INTERVAL: usize = 250;
const HASH_PROGRESS_EMIT_INTERVAL: usize = 25;
const SIMILARITY_PROGRESS_EMIT_INTERVAL: usize = 25;
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
    group_kind: String,
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
    scanned_files: usize,
    duplicate_groups: usize,
    exact_groups: usize,
    reduced_groups: usize,
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
    scanned_files: usize,
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
    image_candidates: Vec<ScanCandidate>,
    hash_candidates: Vec<ScanCandidate>,
    total_files_seen: usize,
    scanned_files: usize,
    image_files: usize,
    skipped_files: usize,
    hash_candidate_files: usize,
}

#[derive(Clone, Debug)]
struct SimilarPhotoRecord {
    signature: u64,
    aspect_ratio_key: String,
    record: PhotoRecord,
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
        collect_file_paths(&roots, use_root_prefix, app, cancel_flag, &mut warnings)?;
    let ScanInventory {
        image_candidates,
        hash_candidates,
        total_files_seen,
        scanned_files,
        image_files,
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
                scanned_files,
                duplicate_groups: 0,
                exact_groups: 0,
                reduced_groups: 0,
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
                "{} fichier(s) pris en charge, {} candidate(s) a hasher sur {} thread(s).",
                scanned_files, hash_candidate_files, hash_parallelism
            ),
            0,
            Some(hash_candidate_files),
            None,
            total_files_seen,
            scanned_files,
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

            match build_hashed_photo_record(root, &candidate.path, use_root_prefix) {
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
                        scanned_files,
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
                    scanned_files,
                    hash_candidate_files,
                    preview_groups,
                ),
            );
        }

        return Err(error);
    }

    let groups = {
        let guard = files_by_hash
            .lock()
            .map_err(|_| "Le regroupement des hash est devenu indisponible.".to_string())?;
        build_duplicate_groups(&guard, None)
    };

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "similarity",
            format!(
                "Verification des copies reduites ou recompressees sur {} image(s).",
                image_files
            ),
            0,
            Some(image_files),
            None,
            total_files_seen,
            scanned_files,
            hash_candidate_files,
            build_duplicate_groups_preview(&groups),
        ),
    );

    let similarity_last_emitted = Arc::new(AtomicUsize::new(0));
    let similarity_processed_counter = Arc::new(AtomicUsize::new(0));
    let similar_records = Arc::new(Mutex::new(Vec::<SimilarPhotoRecord>::new()));

    let similarity_result = pool.install(|| {
        image_candidates
            .par_iter()
            .try_for_each(|candidate| -> Result<(), String> {
                if is_scan_cancelled(cancel_flag) {
                    return Err(SCAN_CANCELLED_MESSAGE.to_string());
                }

                let root = &roots[candidate.root_index];

                match build_similar_photo_record(root, &candidate.path, use_root_prefix) {
                    Ok(Some(record)) => {
                        let mut guard = similar_records.lock().map_err(|_| {
                            "Le buffer de similarite est devenu indisponible.".to_string()
                        })?;
                        guard.push(record);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let mut warning_guard = warning_buffer.lock().map_err(|_| {
                            "Le buffer de warnings est devenu indisponible.".to_string()
                        })?;
                        push_warning(
                            &mut warning_guard,
                            format!("{}: {error}", candidate.path.to_string_lossy()),
                        );
                    }
                }

                let processed_items = similarity_processed_counter.fetch_add(1, Ordering::SeqCst) + 1;

                if should_emit_similarity_progress(processed_items, image_files)
                    && similarity_last_emitted.swap(processed_items, Ordering::SeqCst) < processed_items
                {
                    emit_scan_progress(
                        app,
                        ScanProgressPayload::new(
                            "similarity",
                            format!(
                                "Comparaison visuelle {processed_items}/{image_files}..."
                            ),
                            processed_items,
                            Some(image_files),
                            Some(candidate.display_path.clone()),
                            total_files_seen,
                            scanned_files,
                            hash_candidate_files,
                            build_duplicate_groups_preview(&groups),
                        ),
                    );
                }

                if is_scan_cancelled(cancel_flag) {
                    return Err(SCAN_CANCELLED_MESSAGE.to_string());
                }

                Ok(())
            })
    });

    if let Err(error) = similarity_result {
        if error == SCAN_CANCELLED_MESSAGE {
            emit_scan_progress(
                app,
                ScanProgressPayload::new(
                    "cancelled",
                    "Analyse annulee par l'utilisateur.",
                    similarity_processed_counter.load(Ordering::SeqCst),
                    Some(image_files),
                    None,
                    total_files_seen,
                    scanned_files,
                    hash_candidate_files,
                    build_duplicate_groups_preview(&groups),
                ),
            );
        }

        return Err(error);
    }

    let warnings = Arc::try_unwrap(warning_buffer)
        .map_err(|_| "Impossible de finaliser les warnings du scan.".to_string())?
        .into_inner()
        .map_err(|_| "Le buffer de warnings est devenu indisponible.".to_string())?;

    let duplicate_groups = groups.len();
    let skipped_files = skipped_counter.load(Ordering::SeqCst);

    let similar_records = Arc::try_unwrap(similar_records)
        .map_err(|_| "Impossible de finaliser les signatures de similarite.".to_string())?
        .into_inner()
        .map_err(|_| "Le buffer de similarite est devenu indisponible.".to_string())?;
    let reduced_groups = build_reduced_copy_groups(&groups, similar_records, None);
    let exact_groups = duplicate_groups;
    let reduced_group_count = reduced_groups.len();

    let mut all_groups = groups;
    all_groups.extend(reduced_groups);
    sort_groups_for_display(&mut all_groups);

    let duplicates_to_remove = all_groups.iter().map(|group| group.file_count - 1).sum();
    let reclaimable_bytes = all_groups.iter().map(|group| group.reclaimable_bytes).sum();

    emit_scan_progress(
        app,
        ScanProgressPayload::new(
            "complete",
            format!(
                "Analyse terminee: {exact_groups} doublon(s) exact(s) et {reduced_group_count} copie(s) reduite(s) detecte(s)."
            ),
            scanned_files,
            Some(scanned_files),
            None,
            total_files_seen,
            scanned_files,
            hash_candidate_files,
            build_duplicate_groups_preview(&all_groups),
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
            scanned_files,
            duplicate_groups: all_groups.len(),
            exact_groups,
            reduced_groups: reduced_group_count,
            duplicates_to_remove,
            reclaimable_bytes,
            skipped_files,
        },
        groups: all_groups,
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

fn collect_file_paths(
    roots: &[ScanRoot],
    use_root_prefix: bool,
    app: &AppHandle,
    cancel_flag: &Arc<AtomicBool>,
    warnings: &mut Vec<String>,
) -> Result<ScanInventory, String> {
    let mut size_groups: HashMap<u64, Vec<ScanCandidate>> = HashMap::new();
    let mut image_candidates = Vec::new();
    let mut total_files_seen = 0usize;
    let mut scanned_files = 0usize;
    let mut image_files = 0usize;
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

            match entry.metadata() {
                Ok(metadata) => {
                    scanned_files += 1;
                    let candidate = ScanCandidate {
                        path: entry.path().to_path_buf(),
                        root_index,
                        display_path: display_path_for_root(
                            root,
                            entry.path(),
                            use_root_prefix,
                        ),
                    };
                    if is_image_file(entry.path()) {
                        image_files += 1;
                        image_candidates.push(candidate.clone());
                    }
                    size_groups
                        .entry(metadata.len())
                        .or_default()
                        .push(candidate);
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

            if should_emit_count_progress(total_files_seen) {
                emit_scan_progress(
                    app,
                    ScanProgressPayload::new(
                        "counting",
                        format!(
                            "{} entree(s) parcourue(s), {} fichier(s) retenu(s).",
                            total_files_seen, scanned_files
                        ),
                        total_files_seen,
                        None,
                        Some(display_path_for_root(root, entry.path(), use_root_prefix)),
                        total_files_seen,
                        scanned_files,
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
                "Preparation terminee: {} fichier(s) en file d'attente, {} candidate(s) a hasher.",
                scanned_files, hash_candidate_files
            ),
            total_files_seen,
            None,
            None,
            total_files_seen,
            scanned_files,
            hash_candidate_files,
            Vec::new(),
        ),
    );

    Ok(ScanInventory {
        image_candidates,
        hash_candidates,
        total_files_seen,
        scanned_files,
        image_files,
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
        .flat_map(|(hash, files)| {
            build_exact_groups_for_hash(hash, files)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect();

    sort_groups_for_display(&mut groups);

    if let Some(limit) = limit {
        groups.truncate(limit);
    }

    groups
}

fn build_reduced_copy_groups(
    exact_groups: &[DuplicateGroup],
    similar_records: Vec<SimilarPhotoRecord>,
    limit: Option<usize>,
) -> Vec<DuplicateGroup> {
    let exact_member_paths: HashSet<&str> = exact_groups
        .iter()
        .flat_map(|group| group.files.iter().map(|file| file.path.as_str()))
        .collect();
    let exact_keep_paths: HashSet<&str> = exact_groups
        .iter()
        .map(|group| group.keep_path.as_str())
        .collect();

    let mut representative_records = Vec::new();

    for similar_record in similar_records {
        let path = similar_record.record.path.as_str();
        if exact_member_paths.contains(path) && !exact_keep_paths.contains(path) {
            continue;
        }

        representative_records.push(similar_record);
    }

    let mut buckets: HashMap<String, Vec<PhotoRecord>> = HashMap::new();
    for similar_record in representative_records {
        let bucket_key = format!(
            "visual-{:016x}-{}",
            similar_record.signature, similar_record.aspect_ratio_key
        );
        buckets
            .entry(bucket_key)
            .or_default()
            .push(similar_record.record);
    }

    let mut groups: Vec<DuplicateGroup> = buckets
        .into_iter()
        .filter_map(|(bucket_key, files)| {
            if files.len() < 2 {
                return None;
            }

            let mut files = files;
            files.sort_by(compare_keep_candidates);
            let keep_file = files.first()?.clone();
            let total_size_bytes = files.iter().map(|file| file.size_bytes).sum();
            let reclaimable_bytes = files.iter().skip(1).map(|file| file.size_bytes).sum();

            Some(DuplicateGroup {
                hash: bucket_key,
                group_kind: "reduced".to_string(),
                file_count: files.len(),
                total_size_bytes,
                reclaimable_bytes,
                keep_path: keep_file.path.clone(),
                keep_relative_path: keep_file.relative_path.clone(),
                keep_reason: explain_reduced_copy_choice(&keep_file, &files),
                files,
            })
        })
        .collect();

    sort_groups_for_display(&mut groups);

    if let Some(limit) = limit {
        groups.truncate(limit);
    }

    groups
}

fn build_duplicate_groups_preview(groups: &[DuplicateGroup]) -> Vec<DuplicateGroup> {
    groups.iter().take(MAX_PREVIEW_GROUPS).cloned().collect()
}

fn build_exact_groups_for_hash(hash: &str, files: &[PhotoRecord]) -> Vec<DuplicateGroup> {
    let mut files_by_extension: HashMap<&str, Vec<PhotoRecord>> = HashMap::new();
    for file in files {
        files_by_extension
            .entry(file.extension.as_str())
            .or_default()
            .push(file.clone());
    }

    files_by_extension
        .into_iter()
        .filter_map(|(_, mut exact_files)| {
            if exact_files.len() < 2 {
                return None;
            }

            exact_files.sort_by(compare_keep_candidates);
            let keep_file = exact_files.first()?.clone();
            let total_size_bytes = exact_files.iter().map(|file| file.size_bytes).sum();
            let reclaimable_bytes = exact_files.iter().skip(1).map(|file| file.size_bytes).sum();
            let keep_reason = explain_keep_choice(&keep_file, &exact_files);

            Some(DuplicateGroup {
                hash: hash.to_string(),
                group_kind: "exact".to_string(),
                file_count: exact_files.len(),
                total_size_bytes,
                reclaimable_bytes,
                keep_path: keep_file.path.clone(),
                keep_relative_path: keep_file.relative_path.clone(),
                keep_reason,
                files: exact_files,
            })
        })
        .collect()
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
        return Err("Ajoute au moins un dossier a analyser.".into());
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

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .map(|extension| IMAGE_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

fn ensure_file_accessible_for_scan(path: &Path) -> Result<PathBuf, String> {
    try_prepare_file_for_scan(path).map_err(|error| {
        format!(
            "fichier cloud non synchronise ou inaccessible apres tentative de synchronisation ({error})"
        )
    })?;

    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn try_prepare_file_for_scan(path: &Path) -> Result<(), String> {
    let mut last_error = None;

    for delay_ms in [0u64, 200, 500, 1_000] {
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }

        match probe_file_for_scan(path) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| "acces local impossible".to_string()))
}

fn probe_file_for_scan(path: &Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("ouverture impossible ({error})"))?;
    let mut probe = [0u8; 1];
    file.read(&mut probe)
        .map_err(|error| format!("lecture impossible ({error})"))?;
    fs::metadata(path).map_err(|error| format!("metadata indisponibles ({error})"))?;
    Ok(())
}

fn build_hashed_photo_record(
    root: &ScanRoot,
    path: &Path,
    use_root_prefix: bool,
) -> Result<(String, PhotoRecord), String> {
    let accessible_path = ensure_file_accessible_for_scan(path)?;
    let record = build_photo_record_metadata(root, &accessible_path, use_root_prefix)?;
    let hash = compute_sha256(&accessible_path)?;
    Ok((hash, record))
}

fn build_similar_photo_record(
    root: &ScanRoot,
    path: &Path,
    use_root_prefix: bool,
) -> Result<Option<SimilarPhotoRecord>, String> {
    let accessible_path = ensure_file_accessible_for_scan(path)?;
    let record = build_photo_record_metadata(root, &accessible_path, use_root_prefix)?;
    let Some((signature, aspect_ratio_key)) = compute_visual_signature(
        &accessible_path,
        &record.extension,
        record.width,
        record.height,
    )?
    else {
        return Ok(None);
    };

    Ok(Some(SimilarPhotoRecord {
        signature,
        aspect_ratio_key,
        record,
    }))
}

fn build_photo_record_metadata(
    root: &ScanRoot,
    path: &Path,
    use_root_prefix: bool,
) -> Result<PhotoRecord, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("metadata indisponibles ({error})"))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| "extension manquante".to_string())?;

    let dimensions = read_dimensions(path, &extension);
    let size_bytes = metadata.len();
    let quality_score = compute_quality_score(&extension, dimensions, size_bytes);
    let quality_reason = build_quality_reason(&extension, dimensions, size_bytes);
    let relative_path = display_path_for_root(root, path, use_root_prefix);

    Ok(PhotoRecord {
        path: path_to_string(path),
        relative_path,
        file_name: path
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
    })
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

fn compute_visual_signature(
    path: &Path,
    extension: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<Option<(u64, String)>, String> {
    if !matches!(extension, "jpg" | "jpeg" | "png") {
        return Ok(None);
    }

    let (width, height) = match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
        _ => return Ok(None),
    };

    let image = ImageReader::open(path)
        .map_err(|error| format!("ouverture image impossible ({error})"))?
        .with_guessed_format()
        .map_err(|error| format!("format image inconnu ({error})"))?
        .decode()
        .map_err(|error| format!("decodage image impossible ({error})"))?;

    let thumbnail = image
        .resize_exact(8, 8, FilterType::Triangle)
        .grayscale()
        .to_luma8();
    let average = thumbnail.pixels().map(|pixel| u32::from(pixel.0[0])).sum::<u32>() / 64;
    let mut signature = 0u64;

    for pixel in thumbnail.pixels() {
        signature <<= 1;
        if u32::from(pixel.0[0]) >= average {
            signature |= 1;
        }
    }

    Ok(Some((signature, normalized_aspect_ratio_key(width, height))))
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
        None if is_image_extension(extension) => format!(
            "dimensions non lues, format {}, {} octets",
            extension.to_uppercase(),
            size_bytes
        ),
        None => format!(
            "format {}, {} octets",
            extension.to_uppercase(),
            size_bytes
        ),
    }
}

fn is_image_extension(extension: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&extension)
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

fn explain_reduced_copy_choice(keep_file: &PhotoRecord, files: &[PhotoRecord]) -> String {
    let lower_quality_versions = files.iter().filter(|file| file.path != keep_file.path).count();

    format!(
        "PicMan a reconnu {} version(s) visuellement tres proche(s). `{}` est conservee car elle offre la meilleure definition ou compression du groupe: {}.",
        lower_quality_versions,
        keep_file.relative_path,
        keep_file.quality_reason
    )
}

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn normalized_aspect_ratio_key(width: u32, height: u32) -> String {
    let divisor = greatest_common_divisor(width, height).max(1);
    format!("{}:{}", width / divisor, height / divisor)
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }

    left
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

fn should_emit_similarity_progress(processed_items: usize, total_items: usize) -> bool {
    processed_items == 1
        || processed_items == total_items
        || processed_items % SIMILARITY_PROGRESS_EMIT_INTERVAL == 0
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

fn sort_groups_for_display(groups: &mut [DuplicateGroup]) {
    groups.sort_by(|left, right| {
        right
            .reclaimable_bytes
            .cmp(&left.reclaimable_bytes)
            .then_with(|| right.file_count.cmp(&left.file_count))
            .then_with(|| left.keep_relative_path.cmp(&right.keep_relative_path))
    });
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
        scanned_files: usize,
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
            scanned_files,
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
    fn image_extensions_are_case_insensitive() {
        assert!(is_image_file(Path::new(r"C:\Photos\summer.JPG")));
        assert!(is_image_file(Path::new(r"C:\Photos\portrait.heic")));
        assert!(!is_image_file(Path::new(r"C:\Photos\clip.mp4")));
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

    #[test]
    fn exact_duplicate_groups_require_same_extension() {
        let mut files_by_hash = HashMap::new();
        files_by_hash.insert(
            "same-hash".to_string(),
            vec![
                PhotoRecord {
                    path: r"C:\Files\a.jpg".into(),
                    relative_path: "a.jpg".into(),
                    file_name: "a.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 100,
                    width: None,
                    height: None,
                    modified_unix_ms: None,
                    quality_score: 100,
                    quality_reason: String::new(),
                },
                PhotoRecord {
                    path: r"C:\Files\b.jpg".into(),
                    relative_path: "b.jpg".into(),
                    file_name: "b.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 100,
                    width: None,
                    height: None,
                    modified_unix_ms: None,
                    quality_score: 90,
                    quality_reason: String::new(),
                },
                PhotoRecord {
                    path: r"C:\Files\c.png".into(),
                    relative_path: "c.png".into(),
                    file_name: "c.png".into(),
                    extension: "png".into(),
                    size_bytes: 100,
                    width: None,
                    height: None,
                    modified_unix_ms: None,
                    quality_score: 95,
                    quality_reason: String::new(),
                },
            ],
        );

        let groups = build_duplicate_groups(&files_by_hash, None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hash, "same-hash");
        assert_eq!(groups[0].file_count, 2);
        assert!(groups[0].files.iter().all(|file| file.extension == "jpg"));
    }

    #[test]
    fn reduced_copy_groups_merge_same_visual_signature() {
        let stronger = PhotoRecord {
            path: r"C:\Photos\best.jpg".into(),
            relative_path: r"best.jpg".into(),
            file_name: "best.jpg".into(),
            extension: "jpg".into(),
            size_bytes: 4_000_000,
            width: Some(4000),
            height: Some(3000),
            modified_unix_ms: None,
            quality_score: compute_quality_score("jpg", Some((4000, 3000)), 4_000_000),
            quality_reason: String::new(),
        };
        let reduced = PhotoRecord {
            path: r"C:\Photos\best-small.jpg".into(),
            relative_path: r"best-small.jpg".into(),
            file_name: "best-small.jpg".into(),
            extension: "jpg".into(),
            size_bytes: 800_000,
            width: Some(1600),
            height: Some(1200),
            modified_unix_ms: None,
            quality_score: compute_quality_score("jpg", Some((1600, 1200)), 800_000),
            quality_reason: String::new(),
        };
        let unrelated = PhotoRecord {
            path: r"C:\Photos\other.jpg".into(),
            relative_path: r"other.jpg".into(),
            file_name: "other.jpg".into(),
            extension: "jpg".into(),
            size_bytes: 750_000,
            width: Some(1600),
            height: Some(900),
            modified_unix_ms: None,
            quality_score: compute_quality_score("jpg", Some((1600, 900)), 750_000),
            quality_reason: String::new(),
        };

        let groups = build_reduced_copy_groups(
            &[],
            vec![
                SimilarPhotoRecord {
                    signature: 0x1234,
                    aspect_ratio_key: "4:3".into(),
                    record: stronger.clone(),
                },
                SimilarPhotoRecord {
                    signature: 0x1234,
                    aspect_ratio_key: "4:3".into(),
                    record: reduced.clone(),
                },
                SimilarPhotoRecord {
                    signature: 0x1234,
                    aspect_ratio_key: "16:9".into(),
                    record: unrelated,
                },
            ],
            None,
        );

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_kind, "reduced");
        assert_eq!(groups[0].keep_path, stronger.path);
        assert_eq!(groups[0].file_count, 2);
    }
}
