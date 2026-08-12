import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useHotkeyBindings } from "../state/hotkeys";
import { useUI } from "../state/ui";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

const STATIC_GROUPS: { title: string; keys: [string, string][] }[] = [
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

/** Every shortcut in one place — discoverability beats memory. The "Navigate"
 * group reflects whatever is actually bound right now, since every entry in
 * it (except Alt+1…9) is rebindable from Settings → Keyboard shortcuts. */
export function HotkeysDialog({ onClose }: { onClose: () => void }) {
  useEscapeToClose(onClose);
  const comboFor = useHotkeyBindings((s) => s.comboFor);
  const navigateKeys: [string, string][] = [
    ["Alt+1…9", "Select the nth worktree"],
    [`${comboFor("worktree-prev")} / ${comboFor("worktree-next")}`, "Previous / next worktree"],
    [
      `${comboFor("tab-chat")} / ${comboFor("tab-diff")} / ${comboFor("tab-notes")}`,
      "Chat / Diff / Notes tab",
    ],
    [comboFor("command-palette"), "Command palette"],
  ];
  const groups = [{ title: "Navigate", keys: navigateKeys }, ...STATIC_GROUPS];
  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="sliders" /> Keyboard shortcuts
        </h3>
        {groups.map((group) => (
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
          <button
            className="small ghost"
            onClick={() => {
              onClose();
              useUI.getState().openDialog("settings");
            }}
          >
            <Icon name="settings" size={12} /> Customize…
          </button>
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
