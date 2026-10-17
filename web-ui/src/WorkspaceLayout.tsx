import { useEffect, useRef, useState, type CSSProperties } from "react";
import { readPreference, writePreference } from "./preferences";

export function useWorkspaceLayout() {
  const workspaceRef = useRef<HTMLElement>(null);
  const [layout, setLayout] = useState(() => {
    return readPreference("layout") ?? { width: 330, side: "right" as const };
  });
  useEffect(() => {
    writePreference("layout", layout);
  }, [layout]);
  const resize = (width: number) => {
    const available = workspaceRef.current?.clientWidth ?? window.innerWidth;
    setLayout((previous) => ({
      ...previous,
      width: Math.max(240, Math.min(640, available - 300, width)),
    }));
  };
  return {
    workspaceRef,
    side: layout.side,
    style: { "--explorer-width": `${layout.width}px` } as CSSProperties,
    toggle: () =>
      setLayout((previous) => ({
        ...previous,
        side: previous.side === "left" ? "right" : "left",
      })),
    separator: (
      <div
        className="pane-resizer"
        role="separator"
        tabIndex={0}
        aria-label="Repository browser width"
        aria-orientation="vertical"
        aria-valuemin={240}
        aria-valuemax={640}
        aria-valuenow={layout.width}
        onKeyDown={(event) => {
          if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
            event.preventDefault();
            resize(
              layout.width +
                (event.key === "ArrowRight" ? 20 : -20) * (layout.side === "left" ? 1 : -1),
            );
          } else if (event.key === "Home") resize(240);
          else if (event.key === "End") resize(640);
        }}
        onPointerDown={(event) => {
          event.currentTarget.setPointerCapture(event.pointerId);
          event.preventDefault();
        }}
        onPointerMove={(event) => {
          if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
          const bounds = workspaceRef.current?.getBoundingClientRect();
          if (bounds)
            resize(
              layout.side === "left" ? event.clientX - bounds.left : bounds.right - event.clientX,
            );
        }}
        onPointerUp={(event) => {
          if (event.currentTarget.hasPointerCapture(event.pointerId))
            event.currentTarget.releasePointerCapture(event.pointerId);
        }}
      />
    ),
  };
}
