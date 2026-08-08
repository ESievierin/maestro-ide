/**
 * Inline SVG icon set. Everything is stroke-based on `currentColor` at a single weight,
 * so icons inherit the colour of the badge, button, or text they sit in and stay
 * consistent across the app. No icon font, no network requests.
 */

import type { ReactElement } from "react";

export type IconName =
  | "branch"
  | "chat"
  | "diff"
  | "shield"
  | "bell"
  | "file-text"
  | "refresh"
  | "close"
  | "check"
  | "alert"
  | "play"
  | "stop"
  | "trash"
  | "plus"
  | "spinner"
  | "arrow-up"
  | "arrow-down"
  | "question"
  | "folder"
  | "chevron-down"
  | "circle"
  | "square"
  | "sliders"
  | "copy"
  | "external-link"
  | "history"
  | "upload"
  | "log"
  | "bot";

/** Path data per icon, drawn on a 24×24 grid. */
const PATHS: Record<IconName, ReactElement> = {
  branch: (
    <>
      <circle cx="6" cy="6" r="2.5" />
      <circle cx="6" cy="18" r="2.5" />
      <circle cx="18" cy="9" r="2.5" />
      <path d="M6 8.5v7M8.5 6h4A3 3 0 0 1 15.5 9M18 11.5v1A3 3 0 0 1 15 15.5H9" />
    </>
  ),
  chat: <path d="M4 5.5h16v10H9l-5 4v-14Z" />,
  diff: <path d="M5 4v11M5 15a3 3 0 0 0 3 3h4M19 20V9M19 9a3 3 0 0 0-3-3h-4M9 12H2.5M21.5 15H15" />,
  shield: <path d="M12 3.5 5 6v6c0 4 3 7 7 8.5 4-1.5 7-4.5 7-8.5V6l-7-2.5Z" />,
  bell: <path d="M6 9a6 6 0 0 1 12 0c0 4 1.5 6 1.5 6h-15S6 13 6 9ZM10 19a2 2 0 0 0 4 0" />,
  "file-text": <path d="M6 3.5h8l4 4v13H6v-17ZM14 3.5v4h4M9 12h6M9 15.5h6M9 8.5h2" />,
  refresh: <path d="M20 6v5h-5M4 18v-5h5M19 11a7 7 0 0 0-12-4L4 9M5 13a7 7 0 0 0 12 4l3-2" />,
  close: <path d="M6 6l12 12M18 6 6 18" />,
  check: <path d="M5 13l4.5 4.5L19 7" />,
  alert: <path d="M12 4.5 21 19H3l9-14.5ZM12 10v4M12 16.5v.5" />,
  play: <path d="M8 5.5v13l11-6.5-11-6.5Z" />,
  stop: <rect x="6.5" y="6.5" width="11" height="11" rx="1.5" />,
  trash: <path d="M4.5 7h15M9 7V4.5h6V7M6.5 7l1 13h9l1-13M10 11v6M14 11v6" />,
  plus: <path d="M12 5v14M5 12h14" />,
  spinner: <path d="M12 4a8 8 0 0 1 8 8" />,
  "arrow-up": <path d="M12 19V5M6 11l6-6 6 6" />,
  "arrow-down": <path d="M12 5v14M6 13l6 6 6-6" />,
  folder: <path d="M3.5 6.5h5l2 2.5h10v9.5h-17V6.5Z" />,
  "chevron-down": <path d="M6 10l6 6 6-6" />,
  question: <path d="M9 9a3 3 0 1 1 4.5 2.6c-1 .6-1.5 1.2-1.5 2.4M12 17.5v.5" />,
  circle: <circle cx="12" cy="12" r="7" />,
  square: <rect x="5.5" y="5.5" width="13" height="13" rx="2.5" />,
  sliders: <path d="M4 7h9M17 7h3M4 17h3M11 17h9M15 4.5v5M9 14.5v5" />,
  copy: (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2" />
      <path d="M5 15H4.5A1.5 1.5 0 0 1 3 13.5v-9A1.5 1.5 0 0 1 4.5 3h9A1.5 1.5 0 0 1 15 4.5V5" />
    </>
  ),
  "external-link": (
    <path d="M13 4.5h6.5V11M19 5 10.5 13.5M15.5 13v5.5a1.5 1.5 0 0 1-1.5 1.5H5.5A1.5 1.5 0 0 1 4 18.5V9a1.5 1.5 0 0 1 1.5-1.5H11" />
  ),
  history: <path d="M4.5 12a7.5 7.5 0 1 1 2.2 5.3M4.5 12l-2-2M4.5 12l2.2-1.8M12 8.5V12l2.7 1.8" />,
  upload: <path d="M4.5 19.5h15M12 15.5V4.5M7 9.5l5-5 5 5" />,
  log: <path d="M6 5.5h12M6 10h12M6 14.5h8M6 19h5" />,
  bot: (
    <>
      <rect x="5" y="8" width="14" height="10" rx="2.5" />
      <path d="M12 8V4.5M12 4.5h.01M9.5 21h5M9 18v3M15 18v3" />
      <circle cx="9.5" cy="12.5" r="0.6" fill="currentColor" />
      <circle cx="14.5" cy="12.5" r="0.6" fill="currentColor" />
    </>
  ),
};

interface Props {
  name: IconName;
  /** Pixel size; icons are square. */
  size?: number;
  /** Adds a continuous rotation (used for the spinner). */
  spin?: boolean;
  className?: string;
}

export function Icon({ name, size = 14, spin = false, className }: Props) {
  return (
    <svg
      className={`icon${spin ? " icon-spin" : ""}${className ? ` ${className}` : ""}`}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.9}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {PATHS[name]}
    </svg>
  );
}

/** Small filled dot used for live/idle state next to a label. */
export function StatusDot({ tone, pulse = false }: { tone: string; pulse?: boolean }) {
  return <span className={`status-dot status-dot-${tone}${pulse ? " pulsing" : ""}`} />;
}
