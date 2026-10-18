export interface TreeActions {
  expandAll: () => void;
  collapseAll: () => void;
}

/** Controlled toolbar shared by file, change and history trees. */
export function TreeControls({
  compact,
  onCompactChange,
  onExpand,
  onCollapse,
  disabled = false,
}: {
  compact: boolean;
  onCompactChange: (compact: boolean) => void;
  onExpand: () => void;
  onCollapse: () => void;
  disabled?: boolean;
}) {
  return (
    <div className="tree-controls" role="group" aria-label="Directory tree controls">
      <button
        aria-label="Compact folders"
        title="Compact folders · combine single-child directories"
        aria-pressed={compact}
        disabled={disabled}
        onClick={() => onCompactChange(!compact)}
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 20 20"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.4"
          aria-hidden="true"
        >
          <path d="M3 4h5v4h5v4h4M3 4v12h5M8 8v8h5" />
          <path d="M13 12v4h4" />
        </svg>
      </button>
      <button
        aria-label="Expand folders"
        title="Expand folders"
        disabled={disabled}
        onClick={onExpand}
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 20 20"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.4"
          aria-hidden="true"
        >
          <path d="m6 6 4-3 4 3M10 3v5m-4 6 4 3 4-3m-4 3v-5M3 10h14" />
        </svg>
      </button>
      <button
        aria-label="Collapse folders"
        title="Collapse folders"
        disabled={disabled}
        onClick={onCollapse}
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 20 20"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.4"
          aria-hidden="true"
        >
          <path d="m6 3 4 3 4-3M10 1v5m-4 11 4-3 4 3m-4-3v5M3 10h14" />
        </svg>
      </button>
    </div>
  );
}
