import { useEffect, useId, useRef, useState } from "react";

export interface TreeActions {
  expandAll: () => void;
  collapseAll: () => void;
}

/** Options menu shared by file, change and history trees. */
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
  const [open, setOpen] = useState(false);
  const control = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const initialItem = useRef(0);
  const menuId = useId();

  useEffect(() => {
    if (!open) return;
    const items = menu.current?.querySelectorAll<HTMLButtonElement>("button");
    items?.[initialItem.current === -1 ? items.length - 1 : 0]?.focus();
    const dismiss = (event: PointerEvent) => {
      if (event.target instanceof Node && !control.current?.contains(event.target)) setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    return () => document.removeEventListener("pointerdown", dismiss);
  }, [open]);

  const choose = (action: () => void) => {
    action();
    setOpen(false);
    trigger.current?.focus();
  };

  return (
    <div
      className="tree-controls"
      ref={control}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false);
      }}
      onKeyDown={(event) => {
        if (!open || event.key !== "Escape") return;
        event.preventDefault();
        event.stopPropagation();
        setOpen(false);
        trigger.current?.focus();
      }}
    >
      <button
        className="tree-menu-trigger"
        ref={trigger}
        aria-label="Tree options"
        title="Tree options"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        disabled={disabled}
        onClick={() => {
          initialItem.current = 0;
          setOpen((previous) => !previous);
        }}
        onKeyDown={(event) => {
          if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
          event.preventDefault();
          initialItem.current = event.key === "ArrowUp" ? -1 : 0;
          setOpen(true);
        }}
      >
        <svg width="16" height="16" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true">
          <circle cx="4" cy="10" r="1.4" />
          <circle cx="10" cy="10" r="1.4" />
          <circle cx="16" cy="10" r="1.4" />
        </svg>
      </button>
      {open && (
        <div
          className="tree-menu"
          ref={menu}
          id={menuId}
          role="menu"
          aria-label="Directory tree options"
          onKeyDown={(event) => {
            if (event.key === "Tab") {
              setOpen(false);
              trigger.current?.focus();
              return;
            }
            const items = Array.from(
              event.currentTarget.querySelectorAll<HTMLButtonElement>("button"),
            );
            const index = items.indexOf(event.target as HTMLButtonElement);
            const next =
              event.key === "ArrowDown"
                ? (index + 1) % items.length
                : event.key === "ArrowUp"
                  ? (index + items.length - 1) % items.length
                  : event.key === "Home"
                    ? 0
                    : event.key === "End"
                      ? items.length - 1
                      : undefined;
            if (next === undefined) return;
            event.preventDefault();
            items[next].focus();
          }}
        >
          <button
            role="menuitemcheckbox"
            aria-checked={compact}
            tabIndex={-1}
            onClick={() => choose(() => onCompactChange(!compact))}
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
            <span>Compact folders</span>
            <svg
              className="tree-menu-check"
              width="14"
              height="14"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              aria-hidden="true"
            >
              {compact && <path d="m3 8 3 3 7-7" />}
            </svg>
          </button>
          <button role="menuitem" tabIndex={-1} onClick={() => choose(onExpand)}>
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
            <span>Expand folders</span>
          </button>
          <button role="menuitem" tabIndex={-1} onClick={() => choose(onCollapse)}>
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
            <span>Collapse folders</span>
          </button>
        </div>
      )}
    </div>
  );
}
