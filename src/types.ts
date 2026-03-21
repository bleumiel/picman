export interface PhotoRecord {
  path: string;
  relativePath: string;
  fileName: string;
  extension: string;
  sizeBytes: number;
  width: number | null;
  height: number | null;
  modifiedUnixMs: number | null;
  qualityScore: number;
  qualityReason: string;
}

export interface DuplicateGroup {
  hash: string;
  groupKind: "exact" | "reduced";
  fileCount: number;
  totalSizeBytes: number;
  reclaimableBytes: number;
  keepPath: string;
  keepRelativePath: string;
  keepReason: string;
  files: PhotoRecord[];
}

export interface ScanSummary {
  totalFilesSeen: number;
  supportedFiles: number;
  duplicateGroups: number;
  exactGroups: number;
  reducedGroups: number;
  duplicatesToRemove: number;
  reclaimableBytes: number;
  skippedFiles: number;
}

export interface ScanReport {
  rootPaths: string[];
  quarantineRoots: string[];
  summary: ScanSummary;
  groups: DuplicateGroup[];
  warnings: string[];
}

export interface ScanProgress {
  phase: string;
  message: string;
  processedItems: number;
  totalItems: number | null;
  currentPath: string | null;
  totalFilesSeen: number;
  supportedFiles: number;
  hashCandidateFiles: number;
  previewGroups: DuplicateGroup[];
}

export interface QuarantineFailure {
  sourcePath: string;
  reason: string;
}

export interface QuarantineResult {
  quarantinePaths: string[];
  movedCount: number;
  failed: QuarantineFailure[];
}

export interface ThumbnailProps {
  path: string;
  alt: string;
  badge?: string;
  className?: string;
}

export type ResultSortKey =
  | "reclaimable-desc"
  | "files-desc"
  | "keep-path-asc"
  | "keep-path-desc"
  | "modified-desc"
  | "modified-asc";
