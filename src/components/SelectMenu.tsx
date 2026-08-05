import { useEffect, useRef, useState } from "react";
import { Icon, type IconName } from "./Icon";

export interface SelectMenuOption {
  value: string;
  label: string;
  /** Shown greyed-out under the label — a one-line hint of what the option does. */
  description?: string;
}

/**
 * A styled dropdown that replaces a native `<select>` — same idea, but the popup
 * is themed instead of falling back to the OS's (often light-mode) native list,
 * and each option can carry a short description under its label. Meant to sit
 * inside a `.segmented` cluster: the trigger takes on that cluster's flat,
 * divider-separated look instead of its own border.
 */
export function SelectMenu({
  icon,
  value,
  options,
  placeholder,
  onChange,
  title,
  disabled,
}: {
  icon?: IconName;
  value: string;
  options: readonly SelectMenuOption[];
  placeholder?: string;
  onChange: (value: string) => void;
  title?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(() =>
    Math.max(
      0,
      options.findIndex((o) => o.value === value),
    ),
  );
  const rootRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  useEffect(() => {
    if (open)
      setHighlight(
        Math.max(
          0,
          options.findIndex((o) => o.value === value),
        ),
      );
  }, [open, options, value]);

  useEffect(() => {
    if (!open) return;
    listRef.current?.children[highlight]?.scrollIntoView({ block: "nearest" });
  }, [open, highlight]);

  const current = options.find((o) => o.value === value);

  return (
    <div className={`select-menu ${open ? "open" : ""}`} ref={rootRef}>
      <button
        type="button"
        className="select-menu-trigger"
        title={title}
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {icon && <Icon name={icon} size={12} className="select-menu-icon" />}
        <span className="select-menu-value">{current?.label ?? placeholder ?? value}</span>
        <Icon name="chevron-down" size={11} className="select-menu-chevron" />
      </button>
      {open && (
        <ul
          className="select-menu-list"
          role="listbox"
          ref={listRef}
          tabIndex={-1}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setHighlight((h) => (h + 1) % options.length);
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setHighlight((h) => (h - 1 + options.length) % options.length);
            } else if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              const picked = options[highlight];
              if (picked) onChange(picked.value);
              setOpen(false);
            } else if (e.key === "Escape") {
              e.preventDefault();
              setOpen(false);
            } else if (e.key === "Tab") {
              setOpen(false);
            }
          }}
        >
          {options.map((o, i) => (
            <li
              key={o.value}
              role="option"
              aria-selected={o.value === value}
              className={`select-menu-option ${o.value === value ? "selected" : ""} ${i === highlight ? "highlight" : ""}`}
              onMouseEnter={() => setHighlight(i)}
              onClick={() => {
                onChange(o.value);
                setOpen(false);
              }}
            >
              <span className="select-menu-option-label">
                {o.label}
                {o.description && <span className="select-menu-option-desc">{o.description}</span>}
              </span>
              {o.value === value && <Icon name="check" size={12} />}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
