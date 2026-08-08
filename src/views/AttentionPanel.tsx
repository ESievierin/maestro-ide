import { Icon, type IconName } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useAttention } from "../state/attention";
import { useWorktrees } from "../state/worktrees";
import type { AttentionItem } from "../types/attention";
import { KIND_LABEL } from "../types/attention";

/**
 * One queue for everything that blocks the fleet: gated commands, inline permission
 * prompts, failed sessions, line-question answers. Clicking an item navigates to where
 * it can be handled — the gate dialog is modal and already on screen, so a gate item
 * just needs acknowledging there.
 */
const KIND_ICON: Record<AttentionItem["kind"], IconName> = {
  gate: "shield",
  permission_request: "question",
  session_failed: "alert",
  line_question: "chat",
};

export function AttentionPanel({ onClose }: { onClose: () => void }) {
  const { items, notificationsEnabled, error, dismiss, setNotificationsEnabled, clearError } =
    useAttention();
  const select = useWorktrees((s) => s.select);
  const setTab = useWorktrees((s) => s.setTab);
  useEscapeToClose(onClose);

  const navigate = (item: AttentionItem) => {
    if (item.branch) {
      select(item.branch);
      setTab(item.target === "diff" ? "diff" : "chat");
    }
    if (item.target !== "gate") {
      // The gate dialog stays until answered; other items are handled by looking.
      void dismiss(item.id);
    }
    onClose();
  };

  return (
    <div className="attention-panel">
      <div className="panel-header">
        <h2>
          Needs you <span className="count">({items.length})</span>
        </h2>
        <div className="actions">
          <label className="notify-toggle" title="OS notifications when an agent blocks">
            <input
              type="checkbox"
              checked={notificationsEnabled}
              onChange={(e) => void setNotificationsEnabled(e.target.checked)}
            />
            notify
          </label>
          <button className="small icon-only" onClick={onClose} title="Close">
            <Icon name="close" />
          </button>
        </div>
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      {items.length === 0 ? (
        <p className="empty">Nothing is waiting on you.</p>
      ) : (
        <ul className="attention-items">
          {items.map((item) => (
            <li key={item.id} className={`attention-${item.kind}`}>
              <button className="attention-main" onClick={() => navigate(item)}>
                <span className={`badge attention-kind-${item.kind}`}>
                  <Icon name={KIND_ICON[item.kind]} size={11} /> {KIND_LABEL[item.kind]}
                </span>
                <span className="attention-message" title={item.message}>
                  {item.message}
                </span>
                <span className="attention-branch">{item.branch ?? ""}</span>
              </button>
              <button
                className="small icon-only"
                title="Dismiss without navigating"
                onClick={() => void dismiss(item.id)}
              >
                <Icon name="close" />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
