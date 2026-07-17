import { useCallback } from "react";

interface ResizeHandleProps {
  axis: "horizontal" | "vertical";
  className?: string;
  label: string;
  onDelta: (delta: number) => void;
}

export function ResizeHandle({ axis, className, label, onDelta }: ResizeHandleProps) {
  const resizeByKeyboard = useCallback(
    (key: string) => {
      const delta = 16;
      if (axis === "vertical" && key === "ArrowLeft") onDelta(-delta);
      else if (axis === "vertical" && key === "ArrowRight") onDelta(delta);
      else if (axis === "horizontal" && key === "ArrowUp") onDelta(-delta);
      else if (axis === "horizontal" && key === "ArrowDown") onDelta(delta);
      else return false;
      return true;
    },
    [axis, onDelta],
  );

  return (
    <div
      aria-label={label}
      aria-orientation={axis}
      className={`resize-handle is-${axis}${className ? ` ${className}` : ""}`}
      onKeyDown={(event) => {
        if (resizeByKeyboard(event.key)) event.preventDefault();
      }}
      onPointerDown={(event) => {
        event.preventDefault();
        let previous = axis === "vertical" ? event.clientX : event.clientY;
        document.body.classList.add(
          axis === "vertical" ? "is-resizing-columns" : "is-resizing-rows",
        );
        const move = (moveEvent: PointerEvent) => {
          const current = axis === "vertical" ? moveEvent.clientX : moveEvent.clientY;
          onDelta(current - previous);
          previous = current;
        };
        const stop = () => {
          document.body.classList.remove("is-resizing-columns", "is-resizing-rows");
          window.removeEventListener("pointermove", move);
          window.removeEventListener("pointerup", stop);
          window.removeEventListener("pointercancel", stop);
        };
        window.addEventListener("pointermove", move);
        window.addEventListener("pointerup", stop, { once: true });
        window.addEventListener("pointercancel", stop, { once: true });
      }}
      role="separator"
      tabIndex={0}
    >
      <span />
    </div>
  );
}
