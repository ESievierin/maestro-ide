import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";

const GROUPS: { title: string; keys: [string, string][] }[] = [
  {
    title: "Navigate",
    keys: [
      ["Alt+1…9", "Select the nth worktree"],
      ["Alt+↑ / Alt+↓", "Previous / next worktree"],
      ["Alt+C / Alt+D / Alt+N", "Chat / Diff / Notes tab"],
      ["Ctrl+K", "Command palette"],
    ],
  },
  {
    title: "Chat",
    keys: [
      ["Enter", "Send the message"],
      ["Shift+Enter", "New line"],
      ["Ctrl+Enter", "Send (alternative)"],
      ["↑ / ↓ at the edges", "Recall previously sent messages"],
      ["/", "Slash-command autocomplete"],
    ],
  },
  {
    title: "Diff review",
    keys: [
      ["Ctrl+↓ / Ctrl+↑ (in the editor)", "Next / previous changed chunk, then next file"],
      ["Ctrl+Enter (commit box)", "Commit"],
      ["Select lines", "Ask the agent about them"],
    ],
  },
  {
    title: "Everywhere",
    keys: [["Esc", "Close the open dialog"]],
  },
];

/** Every shortcut in one place — discoverability beats memory. */
export function HotkeysDialog({ onClose }: { onClose: () => void }) {
  useEscapeToClose(onClose);
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="sliders" /> Keyboard shortcuts
        </h3>
        {GROUPS.map((group) => (
          <div key={group.title} className="hotkeys-group">
            <h4>{group.title}</h4>
            <dl className="hotkeys-list">
              {group.keys.map(([combo, what]) => (
                <div key={combo} className="hotkeys-row">
                  <dt>
                    <kbd>{combo}</kbd>
                  </dt>
                  <dd>{what}</dd>
                </div>
              ))}
            </dl>
          </div>
        ))}
        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
