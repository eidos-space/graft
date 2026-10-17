import type { KeyboardEvent } from "react";

/** Compact tabs with one keyboard stop and automatic arrow-key activation. */
export function SegmentedTabs<Value extends string>({
  id,
  label,
  panel,
  value,
  options,
  onChange,
}: {
  id: string;
  label: string;
  panel: string;
  value: Value;
  options: { value: Value; label: string; count?: number }[];
  onChange: (value: Value) => void;
}) {
  const navigate = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    let next: number;
    if (event.key === "ArrowRight") next = (index + 1) % options.length;
    else if (event.key === "ArrowLeft") next = (index + options.length - 1) % options.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = options.length - 1;
    else return;
    event.preventDefault();
    onChange(options[next].value);
    event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>("button")[next]?.focus();
  };
  return (
    <div className="segmented-tabs" role="tablist" aria-label={label}>
      {options.map((option, index) => (
        <button
          key={option.value}
          id={`${id}-${option.value}`}
          role="tab"
          aria-selected={value === option.value}
          aria-controls={panel}
          tabIndex={value === option.value ? 0 : -1}
          onClick={() => onChange(option.value)}
          onKeyDown={(event) => navigate(event, index)}
        >
          {option.label}
          {option.count !== undefined && (
            <span className="tab-total">{option.count.toLocaleString()}</span>
          )}
        </button>
      ))}
    </div>
  );
}
