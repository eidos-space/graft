import { useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

function ShortcutDialog({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const opener = useRef(document.activeElement as HTMLElement | null);
  const title = useId();
  useLayoutEffect(() => {
    dialog.current?.showModal();
    return () => {
      dialog.current?.close();
      opener.current?.isConnected && opener.current.focus({ preventScroll: true });
    };
  }, []);
  const shortcuts = [
    ["Filter tree / commits", "⌘ / Ctrl + ⇧ + F"],
    ["Toggle sidebar", "⌘ / Ctrl + \\"],
    ["Switch theme", "⌘ / Ctrl + ⇧ + L"],
    ["Save text edits", "⌘ / Ctrl + S"],
    ["Close dialog / filter · Cancel edit", "Esc"],
    ["Resize focused divider", "← / →"],
    ["Divider minimum / maximum width", "Home / End"],
    ["Switch focused tabs", "← / →"],
    ["Expand focused conflict cell", "Enter"],
    ["Choose current / incoming in conflict cell", "Space"],
    ["Choose whole conflict row from row header", "Enter / Space"],
  ];
  return createPortal(
    <dialog
      ref={dialog}
      className="shortcut-dialog"
      aria-labelledby={title}
      onCancel={(event) => {
        event.preventDefault();
        event.stopPropagation();
        onClose();
      }}
      onKeyDown={(event) => event.stopPropagation()}
      onClick={(event) => {
        if (event.target !== event.currentTarget) return;
        const bounds = event.currentTarget.getBoundingClientRect();
        if (
          event.clientX < bounds.left ||
          event.clientX > bounds.right ||
          event.clientY < bounds.top ||
          event.clientY > bounds.bottom
        )
          onClose();
      }}
    >
      <header>
        <h2 id={title}>Keyboard shortcuts</h2>
        <button autoFocus aria-label="Close keyboard shortcuts" onClick={onClose}>
          ×
        </button>
      </header>
      <dl>
        {shortcuts.map(([label, keys]) => (
          <div key={label}>
            <dt>{label}</dt>
            <dd>
              <kbd>{keys}</kbd>
            </dd>
          </div>
        ))}
      </dl>
    </dialog>,
    document.body,
  );
}
export function KeyboardHelp() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        className="sidebar-toggle keyboard-help"
        aria-label="Keyboard shortcuts"
        title="Keyboard shortcuts"
        aria-haspopup="dialog"
        onClick={() => setOpen(true)}
      >
        ⌨
      </button>
      {open && <ShortcutDialog onClose={() => setOpen(false)} />}
    </>
  );
}
