import { FormEvent, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";
import type { QuarantineResult, ScanProgress, ScanReport, ThumbnailProps } from "./types";

const SCAN_PROGRESS_EVENT = "scan-progress";

function App() {
  const [rootPath, setRootPath] = useState("");
  const [report, setReport] = useState<ScanReport | null>(null);
  const [scanProgress, setScanProgress] = useState<ScanProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [isScanning, setIsScanning] = useState(false);
  const [isQuarantining, setIsQuarantining] = useState(false);
  const [lastQuarantineResult, setLastQuarantineResult] = useState<QuarantineResult | null>(null);

  const removablePaths = useMemo(
    () =>
      report
        ? report.groups.flatMap((group) =>
            group.files.filter((file) => file.path !== group.keepPath).map((file) => file.path),
          )
        : [],
    [report],
  );

  const liveGroups = scanProgress?.previewGroups ?? [];
  const displayedGroups = report?.groups ?? liveGroups;

  useEffect(() => {
    let isMounted = true;
    let unlisten: (() => void) | undefined;

    const setupProgressListener = async () => {
      unlisten = await listen<ScanProgress>(SCAN_PROGRESS_EVENT, (event) => {
        if (isMounted) {
          setScanProgress(event.payload);
        }
      });
    };

    void setupProgressListener();

    return () => {
      isMounted = false;
      unlisten?.();
    };
  }, []);

  async function runScan(path: string) {
    return invoke<ScanReport>("scan_photo_library", { rootPath: path });
  }

  async function handlePickDirectory() {
    setError(null);

    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Choisir un dossier photo",
      });

      if (typeof selected === "string") {
        setRootPath(selected);
      }
    } catch (pickerError) {
      setError(normalizeError(pickerError));
    }
  }

  async function refreshScan(path: string) {
    const nextReport = await runScan(path);
    setReport(nextReport);
    setScanProgress(null);
    return nextReport;
  }

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const trimmedPath = rootPath.trim();
    setError(null);
    setSuccess(null);
    setLastQuarantineResult(null);

    if (!trimmedPath) {
      setError("Saisis un chemin absolu Windows vers un dossier photo.");
      return;
    }

    setIsScanning(true);
    setReport(null);
    setScanProgress(createPendingProgress("Preparation de l'analyse du dossier..."));
    try {
      await refreshScan(trimmedPath);
    } catch (scanError) {
      setReport(null);
      setScanProgress(null);
      setError(normalizeError(scanError));
    } finally {
      setIsScanning(false);
    }
  }

  async function quarantinePaths(paths: string[]) {
    const trimmedPath = rootPath.trim();
    if (!trimmedPath || paths.length === 0) {
      return;
    }

    setError(null);
    setSuccess(null);
    setIsQuarantining(true);
    try {
      const result = await invoke<QuarantineResult>("quarantine_duplicates", {
        rootPath: trimmedPath,
        paths,
      });

      setLastQuarantineResult(result);
      setScanProgress(createPendingProgress("Reanalyse du dossier apres quarantaine..."));
      const refreshed = await refreshScan(trimmedPath);
      setSuccess(
        result.failed.length === 0
          ? `${result.movedCount} fichier(s) ont ete deplaces vers ${result.quarantinePath}.`
          : `${result.movedCount} fichier(s) deplaces vers ${result.quarantinePath}. ${result.failed.length} echec(s) restent a verifier.`,
      );

      if (refreshed.groups.length === 0 && result.movedCount > 0) {
        setSuccess(
          `${result.movedCount} fichier(s) ont ete deplaces vers ${result.quarantinePath}. Aucun doublon exact restant.`,
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
          <h1>Nettoyer ses dossiers photo sans supprimer a l&apos;aveugle.</h1>
          <p className="hero-copy">
            Colle un dossier local, lance une analyse des formats JPEG, PNG et HEIC, puis mets
            les doublons exacts en quarantaine en gardant une copie de reference.
          </p>
        </div>
        <div className="hero-card">
          <span className="hero-card-label">Moteur actuel</span>
          <strong>Doublons exacts uniquement</strong>
          <p>Detection par SHA-256, recommandation de conservation et quarantaine reversible.</p>
        </div>
      </section>

      <form className="scan-panel" onSubmit={handleSubmit}>
        <label className="field-label" htmlFor="root-path">
          Dossier a analyser
        </label>
        <div className="scan-row">
          <input
            id="root-path"
            value={rootPath}
            onChange={(event) => setRootPath(event.currentTarget.value)}
            placeholder="C:\\Users\\bleum\\Pictures"
            autoComplete="off"
          />
          <button
            type="button"
            className="secondary-button"
            disabled={isScanning || isQuarantining}
            onClick={handlePickDirectory}
          >
            Parcourir...
          </button>
          <button type="submit" disabled={isScanning || isQuarantining}>
            {isScanning ? "Analyse en cours..." : "Analyser"}
          </button>
        </div>
        <p className="helper-text">
          Exemple: <code>C:\Users\bleum\Pictures</code>. La quarantaine sera creee sous{" "}
          <code>.picman-quarantine</code> dans le dossier scanne.
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
            <span>{scanProgress.supportedFiles} photo(s) prises en charge reperees</span>
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
              label="Photos prises en charge"
              value={report.summary.supportedFiles.toString()}
            />
            <SummaryCard
              label="Groupes de doublons"
              value={report.summary.duplicateGroups.toString()}
            />
            <SummaryCard
              label="Copies a retirer"
              value={report.summary.duplicatesToRemove.toString()}
            />
            <SummaryCard
              label="Espace recuperable"
              value={formatBytes(report.summary.reclaimableBytes)}
            />
            <SummaryCard
              label="Ignores / non pris en charge"
              value={report.summary.skippedFiles.toString()}
            />
          </section>

          <section className="action-panel">
            <div>
              <h2>Action recommande</h2>
              <p>
                PicMan conservera le meilleur candidat de chaque groupe et deplacera les autres
                copies vers <code>{report.quarantineRoot}</code>.
              </p>
              {lastQuarantineResult ? (
                <p className="helper-text">
                  Derniere quarantaine: <code>{lastQuarantineResult.quarantinePath}</code>
                </p>
              ) : null}
            </div>
            <button
              type="button"
              className="danger-button"
              disabled={isScanning || isQuarantining || removablePaths.length === 0}
              onClick={() => quarantinePaths(removablePaths)}
            >
              {isQuarantining ? "Deplacement..." : "Mettre tous les doublons en quarantaine"}
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
                    ? "Aucun doublon exact detecte dans ce dossier."
                    : `${report.groups.length} groupe(s) prets a etre verifies.`}
                </p>
              </div>
            </div>

            {report.groups.length === 0 ? (
              <div className="empty-state">
                <strong>Rien a nettoyer ici.</strong>
                <p>
                  Le scan a termine sans trouver de doublons exacts sur les formats pris en charge.
                </p>
              </div>
            ) : (
              <DuplicateGroupList
                groups={report.groups}
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
            PicMan commencera par parcourir recursivement le dossier, calculer les empreintes des
            images prises en charge puis regrouper les doublons exacts.
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
          </div>

          {displayedGroups.length === 0 ? (
            <div className="empty-state">
              <strong>PicMan continue l'analyse.</strong>
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

        return (
          <article className="group-card" key={group.hash}>
            <header className="group-header">
              <div>
                <p className="group-index">Groupe {index + 1}</p>
                <h3>{shortHash(group.hash)}</h3>
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
                  <p>La miniature principale reprend le fichier recommande a conserver.</p>
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
                      badge={isKept ? "A garder" : "Copie"}
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
                Mettre les autres copies en quarantaine
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
                        <span className={`tag ${isKept ? "keep-tag" : "duplicate-tag"}`}>
                          {isKept ? "A conserver" : "Doublon"}
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
    supportedFiles: 0,
    hashCandidateFiles: 0,
    previewGroups: [],
  };
}

function getProgressTitle(phase: string) {
  switch (phase) {
    case "counting":
      return "Preparation du scan";
    case "hashing":
      return "Analyse des photos";
    case "grouping":
      return "Regroupement des doublons";
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

export default App;
