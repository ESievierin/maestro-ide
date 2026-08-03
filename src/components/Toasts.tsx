import { useToasts } from "../state/toasts";

/** Bottom-right stack of `error.raised` events. Errors stay until dismissed. */
export function Toasts() {
  const toasts = useToasts((s) => s.toasts);
  const dismiss = useToasts((s) => s.dismiss);

  if (toasts.length === 0) return null;

  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.severity}`}>
          <div className="toast-body">
            <span className="toast-code">{t.code}</span>
            <span className="toast-message">{t.message}</span>
          </div>
          <button className="small" title="Dismiss" onClick={() => dismiss(t.id)}>
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
