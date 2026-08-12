import type { MouseEvent } from "react";

/**
 * Backdrop click-to-close, keyed off mousedown rather than click. A click (and the
 * mouseup it's built from) fires on the backdrop even when the gesture started inside
 * the modal — e.g. selecting text and releasing the mouse past the modal's edge — which
 * used to close the dialog out from under whatever the user was doing. Keying off
 * mousedown, and requiring it land directly on the backdrop (not bubbled from a child),
 * only closes when the press itself started outside the modal.
 */
export function closeOnBackdropMouseDown(onClose: () => void) {
  return (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) onClose();
  };
}
