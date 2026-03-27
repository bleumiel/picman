import { FormEvent, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";
import type {
  QuarantineResult,
  ResultSortKey,
  ScanProgress,
  ScanReport,
  ThumbnailProps,
} from "./types";

const SCAN_PROGRESS_EVENT = "scan-progress";

function App() {
  const [rootDraft, setRootDraft] = useState("");
  const [rootPaths, setRootPaths] = useState<string[]>([]);
  const [sortKey, setSortKey] = useState<ResultSortKey>("reclaimable-desc");
  const [report, setReport] = useState<ScanReport | null>(null);
  const [scanProgress, setScanProgress] = useState<ScanProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [isScanning, setIsScanning] = useState(false);
  const [isCancelling, setIsCancelling] = useState(false);
  const [isQuarantining, setIsQuarantining] = useState(false);
  const [lastQuarantineResult, setLastQuarantineResult] = useState<QuarantineResult | null>(null);

  const removablePaths = useMemo(
    () =>
      report
        ? dedupePaths(
            report.groups.flatMap((group) =>
              group.files.filter((file) => file.path !== group.keepPath).map((file) => file.path),
            ),
          )
        : [],
    [report],
  );

  const liveGroups = scanProgress?.previewGroups ?? [];
  const displayedGroups = useMemo(
    () => sortGroups(report?.groups ?? liveGroups, sortKey),
    [liveGroups, report?.groups, sortKey],
  );

  useEffect(() => {
    let isMounted = true;
    let unlisten: (() => void) | undefined;

    const setupProgressListener = async () => {
      unlisten = await listen<ScanProgress>(SCAN_PROGRESS_EVENT, (event) => {
        if (!isMounted) {
          return;
        }

        setScanProgress(event.payload);

        if (event.payload.phase === "cancelled") {
          setIsScanning(false);
          setIsCancelling(false);
          setSuccess("Analyse annulee.");
        }
      });
    };

    void setupProgressListener();

    return () => {
      isMounted = false;
      unlisten?.();
    };
  }, []);

  async function runScan(paths: string[]) {
    return invoke<ScanReport>("scan_photo_library", { rootPaths: paths });
  }

  async function refreshScan(paths: string[]) {
    const nextReport = await runScan(paths);
    setReport(nextReport);
    setScanProgress(null);
    return nextReport;
  }

  async function handlePickDirectory() {
    setError(null);

    try {
      const selected = await open({
        directory: true,
        multiple: true,
        title: "Choisir un ou plusieurs dossiers a analyser",
      });

      if (typeof selected === "string") {
        mergeRootPaths([selected]);
      } else if (Array.isArray(selected)) {
        mergeRootPaths(selected.filter((value): value is string => typeof value === "string"));
      }
    } catch (pickerError) {
      setError(normalizeError(pickerError));
    }
  }

  function mergeRootPaths(paths: string[]) {
    setRootPaths((current) => dedupePaths([...current, ...paths]));
  }

  function removeRootPath(path: string) {
    setRootPaths((current) => current.filter((value) => value !== path));
  }

  function handleAddManualPath() {
    const trimmed = rootDraft.trim();
    if (!trimmed) {
      return;
    }

    mergeRootPaths(splitManualPaths(trimmed));
    setRootDraft("");
  }

  async function handleCancelScan() {
    if (!isScanning || isCancelling) {
      return;
    }

    setError(null);
    setSuccess("Annulation demandee...");
    setIsCancelling(true);

    void invoke<boolean>("cancel_scan").catch((cancelError) => {
      setIsCancelling(false);
      setError(normalizeError(cancelError));
    });
  }

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const selectedRoots = dedupePaths([
      ...rootPaths,
      ...splitManualPaths(rootDraft.trim()),
    ]);

    setError(null);
    setSuccess(null);
    setLastQuarantineResult(null);
    setIsCancelling(false);

    if (selectedRoots.length === 0) {
      setError("Ajoute au moins un dossier a analyser.");
      return;
    }

    setRootPaths(selectedRoots);
    setRootDraft("");
    setIsScanning(true);
    setReport(null);
    setScanProgress(createPendingProgress("Preparation de l'analyse des dossiers..."));

    try {
      await refreshScan(selectedRoots);
    } catch (scanError) {
      setReport(null);
      setScanProgress(null);
      const message = normalizeError(scanError);
      if (message === "Analyse annulee.") {
        setSuccess("Analyse annulee.");
      } else {
        setError(message);
      }
    } finally {
      setIsScanning(false);
      setIsCancelling(false);
    }
  }

  async function quarantinePaths(paths: string[]) {
    if (rootPaths.length === 0 || paths.length === 0) {
      return;
    }

    setError(null);
    setSuccess(null);
    setIsQuarantining(true);
    try {
      const result = await invoke<QuarantineResult>("quarantine_duplicates", {
        rootPaths,
        paths,
      });

      setLastQuarantineResult(result);
      setScanProgress(createPendingProgress("Reanalyse des dossiers apres quarantaine..."));
      const refreshed = await refreshScan(rootPaths);
      const quarantineTargets = formatPathList(result.quarantinePaths);
      setSuccess(
        result.failed.length === 0
          ? `${result.movedCount} fichier(s) ont ete deplaces vers ${quarantineTargets}.`
          : `${result.movedCount} fichier(s) deplaces vers ${quarantineTargets}. ${result.failed.length} echec(s) restent a verifier.`,
      );

      if (refreshed.groups.length === 0 && result.movedCount > 0) {
        setSuccess(
          `${result.movedCount} fichier(s) ont ete deplaces vers ${quarantineTargets}. Aucun doublon exact restant.`,
        );
      }
    } catch (quarantineError) {
      setScanProgress(null);
      setError(normalizeError(quarantineError));
    } finally {
      setIsQuarantining(false);
    }
  }

  const progressPercent = scanProgress ? getProgressPercent(scanProgress) : null;
  const showProgress = scanProgress !== null;

  return (
    <main className="app-shell">
      <section className="hero">
        <div>
          <p className="eyebrow">PicMan MVP</p>
          <h1>Nettoyer ses dossiers sans supprimer a l&apos;aveugle.</h1>
          <p className="hero-copy">
            Selectionne un ou plusieurs dossiers locaux, lance une analyse de tous les fichiers,
            puis mets les doublons exacts en quarantaine en gardant une copie de
            reference.
          </p>
        </div>
        <div className="hero-card">
          <span className="hero-card-label">Moteur actuel</span>
          <strong>Doublons exacts + copies reduites</strong>
          <p>Tous types de fichiers, hash parallele modere, previsualisation live et annulation.</p>
        </div>
      </section>

      <form className="scan-panel" onSubmit={handleSubmit}>
        <label className="field-label" htmlFor="root-path">
          Dossiers a analyser
        </label>
        <div className="scan-row">
          <input
            id="root-path"
            value={rootDraft}
            onChange={(event) => setRootDraft(event.currentTarget.value)}
            placeholder="C:\\Users\\bleum\\Pictures ou plusieurs chemins separes par ;"
            autoComplete="off"
          />
          <button
            type="button"
            className="secondary-button"
            disabled={isScanning || isQuarantining}
            onClick={handleAddManualPath}
          >
            Ajouter
          </button>
          <button
            type="button"
            className="secondary-button"
            disabled={isScanning || isQuarantining}
            onClick={handlePickDirectory}
          >
            Parcourir...
          </button>
          {isScanning ? (
            <button
              type="button"
              className="cancel-button"
              disabled={isCancelling}
              onClick={handleCancelScan}
            >
              {isCancelling ? "Annulation..." : "Annuler"}
            </button>
          ) : null}
          <button type="submit" disabled={isScanning || isQuarantining}>
            {isScanning ? "Analyse en cours..." : "Analyser"}
          </button>
        </div>

        <div className="selected-roots">
          {rootPaths.length === 0 ? (
            <p className="helper-text">
              Ajoute des chemins manuellement ou utilise le picker pour choisir plusieurs dossiers
              a la fois.
            </p>
          ) : (
            rootPaths.map((path) => (
              <button
                key={path}
                type="button"
                className="root-chip"
                disabled={isScanning || isQuarantining}
                onClick={() => removeRootPath(path)}
              >
                <span>{path}</span>
                <strong>&times;</strong>
              </button>
            ))
          )}
        </div>

        <p className="helper-text">
          Les doublons peuvent etre detectes entre plusieurs dossiers. Les suppressions sont
          envoyees vers une quarantaine propre a chaque racine analysee.
        </p>
      </form>

      {showProgress ? (
        <section className="progress-panel">
          <div className="progress-header">
            <div>
              <p className="eyebrow">Progression du scan</p>
              <h2>{getProgressTitle(scanProgress.phase)}</h2>
            </div>
            <span className="progress-badge">
              {progressPercent === null ? "Preparation" : `${progressPercent}%`}
            </span>
          </div>

          <p className="progress-message">{scanProgress.message}</p>

          <div
            className={`progress-bar-track ${
              progressPercent === null ? "indeterminate" : ""
            }`}
          >
            <div
              className="progress-bar-fill"
              style={{ width: `${progressPercent ?? 36}%` }}
            />
          </div>

          <div className="progress-meta">
            <span>{formatProgressCounts(scanProgress)}</span>
            <span>{scanProgress.scannedFiles} fichier(s) pris en charge</span>
            <span>{scanProgress.hashCandidateFiles} candidate(s) a hasher</span>
          </div>

          {scanProgress.currentPath ? (
            <p className="progress-path">
              <strong>Element courant:</strong> {scanProgress.currentPath}
            </p>
          ) : null}
        </section>
      ) : null}

      {error ? (
        <section className="message error-message">
          <strong>Analyse interrompue.</strong>
          <p>{error}</p>
        </section>
      ) : null}

      {success ? (
        <section className="message success-message">
          <strong>Operation terminee.</strong>
          <p>{success}</p>
        </section>
      ) : null}

      {report ? (
        <>
          <section className="summary-grid">
            <SummaryCard label="Fichiers vus" value={report.summary.totalFilesSeen.toString()} />
            <SummaryCard
              label="Fichiers analyses"
              value={report.summary.scannedFiles.toString()}
            />
            <SummaryCard
              label="Doublons exacts"
              value={report.summary.exactGroups.toString()}
            />
            <SummaryCard
              label="Copies reduites"
              value={report.summary.reducedGroups.toString()}
            />
            <SummaryCard
              label="Versions a retirer"
              value={report.summary.duplicatesToRemove.toString()}
            />
            <SummaryCard
              label="Espace recuperable"
              value={formatBytes(report.summary.reclaimableBytes)}
            />
            <SummaryCard
              label="Fichiers inaccessibles"
              value={report.summary.skippedFiles.toString()}
            />
          </section>

          <section className="action-panel">
            <div>
              <h2>Action recommande</h2>
              <p>
                PicMan conservera le meilleur candidat de chaque groupe et deplacera les autres
                versions vers {formatPathList(report.quarantineRoots)}.
              </p>
              {lastQuarantineResult ? (
                <p className="helper-text">
                  Derniere quarantaine: <code>{formatPathList(lastQuarantineResult.quarantinePaths)}</code>
                </p>
              ) : null}
            </div>
            <button
              type="button"
              className="danger-button"
              disabled={isScanning || isQuarantining || removablePaths.length === 0}
              onClick={() => quarantinePaths(removablePaths)}
            >
              {isQuarantining ? "Deplacement..." : "Mettre toutes les versions en quarantaine"}
            </button>
          </section>

          {report.warnings.length > 0 ? (
            <section className="warning-panel">
              <h2>Warnings de scan</h2>
              <ul>
                {report.warnings.map((warning) => (
                  <li key={warning}>{warning}</li>
                ))}
              </ul>
            </section>
          ) : null}

          <section className="results-panel">
            <div className="results-header">
              <div>
                <h2>Groupes detectes</h2>
                <p>
                  {report.groups.length === 0
                    ? "Aucun doublon exact ni copie reduite detecte dans ces dossiers."
                    : formatGroupBreakdown(report.summary)}
                </p>
              </div>
              {report.groups.length > 0 ? (
                <div className="sort-controls">
                  <label htmlFor="result-sort">Trier par</label>
                  <select
                    id="result-sort"
                    value={sortKey}
                    onChange={(event) => setSortKey(event.currentTarget.value as ResultSortKey)}
                  >
                    <option value="reclaimable-desc">Espace recuperable (desc)</option>
                    <option value="files-desc">Nombre de fichiers (desc)</option>
                    <option value="modified-desc">Date du meilleur fichier (recent)</option>
                    <option value="modified-asc">Date du meilleur fichier (ancien)</option>
                    <option value="keep-path-asc">Chemin du fichier garde (A-Z)</option>
                    <option value="keep-path-desc">Chemin du fichier garde (Z-A)</option>
                  </select>
                </div>
              ) : null}
            </div>

            {report.groups.length === 0 ? (
              <div className="empty-state">
                <strong>Rien a nettoyer ici.</strong>
                <p>
                  Le scan a termine sans trouver de doublons exacts.
                </p>
              </div>
            ) : (
              <DuplicateGroupList
                groups={displayedGroups}
                isQuarantining={isQuarantining}
                onQuarantineGroup={quarantinePaths}
              />
            )}
          </section>
        </>
      ) : !isScanning ? (
        <section className="empty-state pre-scan">
          <strong>En attente d&apos;un premier scan.</strong>
          <p>
            PicMan commencera par parcourir les dossiers, calculer les empreintes des fichiers
            puis regrouper les doublons exacts.
          </p>
        </section>
      ) : null}

      {isScanning && !report ? (
        <section className="results-panel">
          <div className="results-header">
            <div>
              <h2>Doublons decouverts en direct</h2>
              <p>
                {displayedGroups.length === 0
                  ? "Aucun groupe detecte pour l'instant."
                  : `${displayedGroups.length} groupe(s) deja identifies pendant l'analyse.`}
              </p>
            </div>
            {displayedGroups.length > 0 ? (
              <div className="sort-controls">
                <label htmlFor="live-result-sort">Trier par</label>
                <select
                  id="live-result-sort"
                  value={sortKey}
                  onChange={(event) => setSortKey(event.currentTarget.value as ResultSortKey)}
                >
                  <option value="reclaimable-desc">Espace recuperable (desc)</option>
                  <option value="files-desc">Nombre de fichiers (desc)</option>
                  <option value="modified-desc">Date du meilleur fichier (recent)</option>
                  <option value="modified-asc">Date du meilleur fichier (ancien)</option>
                  <option value="keep-path-asc">Chemin du fichier garde (A-Z)</option>
                  <option value="keep-path-desc">Chemin du fichier garde (Z-A)</option>
                </select>
              </div>
            ) : null}
          </div>

          {displayedGroups.length === 0 ? (
            <div className="empty-state">
              <strong>PicMan continue l&apos;analyse.</strong>
              <p>
                Les groupes apparaitront ici au fur et a mesure que les fichiers en collision de
                taille seront verifies par hash.
              </p>
            </div>
          ) : (
            <DuplicateGroupList
              groups={displayedGroups}
              isQuarantining={isQuarantining}
              onQuarantineGroup={quarantinePaths}
            />
          )}
        </section>
      ) : null}
    </main>
  );
}

function SummaryCard({ label, value }: { label: string; value: string }) {
  return (
    <article className="summary-card">
      <span>{label}</span>
      <strong>{value}</strong>
    </article>
  );
}

function Thumbnail({ path, alt, badge, className }: ThumbnailProps) {
  const [hasError, setHasError] = useState(false);
  const extension = getFileExtension(path);

  if (hasError) {
    return (
      <div className={`thumbnail-frame fallback ${className ?? ""}`.trim()}>
        {badge ? <span className="thumbnail-badge">{badge}</span> : null}
        <div className="thumbnail-fallback">
          <strong>{extension.toUpperCase() || "IMG"}</strong>
          <span>Miniature indisponible</span>
        </div>
      </div>
    );
  }

  return (
    <div className={`thumbnail-frame ${className ?? ""}`.trim()}>
      {badge ? <span className="thumbnail-badge">{badge}</span> : null}
      <img
        className="thumbnail-image"
        src={convertFileSrc(path)}
        alt={alt}
        loading="lazy"
        onError={() => setHasError(true)}
      />
    </div>
  );
}

function DuplicateGroupList({
  groups,
  isQuarantining,
  onQuarantineGroup,
}: {
  groups: ScanReport["groups"];
  isQuarantining: boolean;
  onQuarantineGroup: (paths: string[]) => void;
}) {
  return (
    <div className="group-list">
      {groups.map((group, index) => {
        const duplicatePaths = group.files
          .filter((file) => file.path !== group.keepPath)
          .map((file) => file.path);
        const groupKindLabel = getGroupKindLabel(group.groupKind);
        const removableLabel = getRemovableLabel(group.groupKind);
        const groupTagClass = group.groupKind === "reduced" ? "derived-tag" : "exact-tag";

        return (
          <article className="group-card" key={group.hash}>
            <header className="group-header">
              <div>
                <p className="group-index">Groupe {index + 1}</p>
                <div className="group-title-row">
                  <h3>{shortHash(group.hash)}</h3>
                  <span className={`tag ${groupTagClass}`}>{groupKindLabel}</span>
                </div>
              </div>
              <div className="group-stats">
                <span>{group.fileCount} fichiers</span>
                <span>{formatBytes(group.reclaimableBytes)} recuperables</span>
              </div>
            </header>

            <section className="group-preview-panel">
              <div className="group-preview-main">
                <Thumbnail
                  path={group.keepPath}
                  alt={`Miniature de ${group.keepRelativePath}`}
                  badge="Reference"
                  className="group-hero-thumbnail"
                />
                <div className="group-preview-copy">
                  <p className="eyebrow">Apercu du groupe</p>
                  <strong>{group.keepRelativePath}</strong>
                  <p>La miniature principale reprend la meilleure version recommandee.</p>
                </div>
              </div>

              <div className="thumbnail-strip">
                {group.files.map((file) => {
                  const isKept = file.path === group.keepPath;

                  return (
                    <Thumbnail
                      key={`${group.hash}-${file.path}`}
                      path={file.path}
                      alt={`Miniature de ${file.relativePath}`}
                      badge={isKept ? "A garder" : removableLabel}
                      className={isKept ? "strip-thumbnail keep" : "strip-thumbnail"}
                    />
                  );
                })}
              </div>
            </section>

            <section className="keep-panel">
              <div>
                <p className="eyebrow">Suggestion a conserver</p>
                <strong>{group.keepRelativePath}</strong>
                <p>{group.keepReason}</p>
              </div>
              <button
                type="button"
                disabled={isQuarantining || duplicatePaths.length === 0}
                onClick={() => onQuarantineGroup(duplicatePaths)}
              >
                Mettre les autres versions en quarantaine
              </button>
            </section>

            <div className="file-list">
              {group.files.map((file) => {
                const isKept = file.path === group.keepPath;

                return (
                  <div className={`file-row ${isKept ? "keep-file" : ""}`} key={file.path}>
                    <div className="file-main">
                      <div className="file-title-row">
                        <strong>{file.relativePath}</strong>
                        <span
                          className={`tag ${
                            isKept
                              ? "keep-tag"
                              : group.groupKind === "reduced"
                                ? "derived-tag"
                                : "duplicate-tag"
                          }`}
                        >
                          {isKept ? "A conserver" : removableLabel}
                        </span>
                      </div>
                      <p>{file.qualityReason}</p>
                    </div>
                    <dl className="file-meta">
                      <div>
                        <dt>Format</dt>
                        <dd>{file.extension.toUpperCase()}</dd>
                      </div>
                      <div>
                        <dt>Dimensions</dt>
                        <dd>{formatDimensions(file.width, file.height)}</dd>
                      </div>
                      <div>
                        <dt>Taille</dt>
                        <dd>{formatBytes(file.sizeBytes)}</dd>
                      </div>
                      <div>
                        <dt>Modifie</dt>
                        <dd>{formatDate(file.modifiedUnixMs)}</dd>
                      </div>
                    </dl>
                  </div>
                );
              })}
            </div>
          </article>
        );
      })}
    </div>
  );
}

function createPendingProgress(message: string): ScanProgress {
  return {
    phase: "counting",
    message,
    processedItems: 0,
    totalItems: null,
    currentPath: null,
    totalFilesSeen: 0,
    scannedFiles: 0,
    hashCandidateFiles: 0,
    previewGroups: [],
  };
}

function getProgressTitle(phase: string) {
  switch (phase) {
    case "counting":
      return "Preparation du scan";
    case "hashing":
      return "Analyse des fichiers";
    case "similarity":
      return "Comparaison visuelle";
    case "grouping":
      return "Regroupement des doublons";
    case "cancelled":
      return "Annulation du scan";
    case "complete":
      return "Analyse terminee";
    default:
      return "Analyse en cours";
  }
}

function getProgressPercent(progress: ScanProgress) {
  if (!progress.totalItems || progress.totalItems <= 0) {
    return null;
  }

  return Math.max(
    0,
    Math.min(100, Math.round((progress.processedItems / progress.totalItems) * 100)),
  );
}

function formatProgressCounts(progress: ScanProgress) {
  if (progress.totalItems && progress.totalItems > 0) {
    return `${progress.processedItems}/${progress.totalItems} candidat(s) hashes`;
  }

  return `${progress.totalFilesSeen} entree(s) parcourue(s)`;
}

function formatBytes(value: number) {
  if (value === 0) {
    return "0 o";
  }

  const units = ["o", "Ko", "Mo", "Go", "To"];
  const unitIndex = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  const converted = value / 1024 ** unitIndex;

  return `${converted.toFixed(converted >= 10 || unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
}

function formatDimensions(width: number | null, height: number | null) {
  if (!width || !height) {
    return "Non lues";
  }

  return `${width} x ${height}`;
}

function formatDate(timestamp: number | null) {
  if (!timestamp) {
    return "Inconnue";
  }

  return new Intl.DateTimeFormat("fr-FR", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp));
}

function shortHash(hash: string) {
  return `${hash.slice(0, 12)}...${hash.slice(-6)}`;
}

function normalizeError(error: unknown) {
  if (typeof error === "string") {
    return error;
  }

  if (error instanceof Error) {
    return error.message;
  }

  return "Une erreur inattendue est survenue.";
}

function getFileExtension(path: string) {
  const segments = path.split(".");
  return segments.length > 1 ? segments[segments.length - 1] ?? "" : "";
}

function splitManualPaths(value: string) {
  return value
    .split(/[;\n\r]+/)
    .map((item) => item.trim())
    .filter(Boolean);
}

function dedupePaths(paths: string[]) {
  const seen = new Set<string>();
  const result: string[] = [];

  for (const path of paths) {
    const key = path.toLowerCase();
    if (!seen.has(key)) {
      seen.add(key);
      result.push(path);
    }
  }

  return result;
}

function formatPathList(paths: string[]) {
  if (paths.length === 0) {
    return "la quarantaine";
  }

  if (paths.length === 1) {
    return paths[0];
  }

  return `${paths[0]} et ${paths.length - 1} autre(s) dossier(s)`;
}

function formatGroupBreakdown(summary: ScanReport["summary"]) {
  const parts = [];

  if (summary.exactGroups > 0) {
    parts.push(`${summary.exactGroups} doublon(s) exact(s)`);
  }

  if (summary.reducedGroups > 0) {
    parts.push(`${summary.reducedGroups} copie(s) reduite(s)`);
  }

  if (parts.length === 0) {
    return "Aucun groupe detecte.";
  }

  return `${parts.join(" et ")} pret(s) a etre verifies.`;
}

function getGroupKindLabel(groupKind: "exact" | "reduced") {
  return groupKind === "reduced" ? "Copie reduite" : "Doublon exact";
}

function getRemovableLabel(groupKind: "exact" | "reduced") {
  return groupKind === "reduced" ? "Version derivee" : "Doublon";
}

function sortGroups(groups: ScanReport["groups"], sortKey: ResultSortKey) {
  const sorted = [...groups];

  sorted.sort((left, right) => {
    switch (sortKey) {
      case "files-desc":
        return (
          right.fileCount - left.fileCount ||
          right.reclaimableBytes - left.reclaimableBytes ||
          left.keepRelativePath.localeCompare(right.keepRelativePath)
        );
      case "keep-path-asc":
        return left.keepRelativePath.localeCompare(right.keepRelativePath);
      case "keep-path-desc":
        return right.keepRelativePath.localeCompare(left.keepRelativePath);
      case "modified-desc":
        return (
          getKeepModifiedTimestamp(right) - getKeepModifiedTimestamp(left) ||
          right.reclaimableBytes - left.reclaimableBytes
        );
      case "modified-asc":
        return (
          getKeepModifiedTimestamp(left) - getKeepModifiedTimestamp(right) ||
          right.reclaimableBytes - left.reclaimableBytes
        );
      case "reclaimable-desc":
      default:
        return (
          right.reclaimableBytes - left.reclaimableBytes ||
          right.fileCount - left.fileCount ||
          left.keepRelativePath.localeCompare(right.keepRelativePath)
        );
    }
  });

  return sorted;
}

function getKeepModifiedTimestamp(group: ScanReport["groups"][number]) {
  const keepFile = group.files.find((file) => file.path === group.keepPath);
  return keepFile?.modifiedUnixMs ?? 0;
}

export default App;
