import { Icon, type IconName } from "../components/Icon";
import { useToasts } from "../state/toasts";

const SEVERITY_ICON: Record<string, IconName> = {
  info: "check",
  warning: "alert",
  error: "alert",
  critical: "alert",
};

/** Bottom-right stack of `error.raised` events. Errors stay until dismissed. */
export function Toasts() {
  const toasts = useToasts((s) => s.toasts);
  const dismiss = useToasts((s) => s.dismiss);

  if (toasts.length === 0) return null;

  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.severity}`}>
          <Icon name={SEVERITY_ICON[t.severity] ?? "alert"} size={15} className="toast-icon" />
          <div className="toast-body">
            <span className="toast-code">{t.code}</span>
            <span className="toast-message">{t.message}</span>
          </div>
          <button className="small icon-only" title="Dismiss" onClick={() => dismiss(t.id)}>
            <Icon name="close" />
          </button>
        </div>
      ))}
    </div>
  );
}
