import { useEffect, useState } from "react";
import { usePrompts } from "../state/prompts";

/**
 * Prompt templates live as markdown files in `~/.maestro/prompts`; this is a plain
 * editor over them. Saving takes effect on the next render in the core — no restart.
 */
export function PromptEditor({ onClose }: { onClose: () => void }) {
  const { templates, loading, error, fetch, save, reset, clearError } = usePrompts();
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void fetch();
  }, [fetch]);

  // Pick the first template once the list arrives.
  useEffect(() => {
    if (!selected && templates.length > 0) setSelected(templates[0].name);
  }, [templates, selected]);

  const current = templates.find((t) => t.name === selected) ?? null;

  // Load the file into the draft whenever the selection or stored content changes.
  useEffect(() => {
    setDraft(current?.content ?? "");
    setSaved(false);
  }, [current?.name, current?.content]);

  const dirty = current !== null && draft !== current.content;

  const doSave = async () => {
    if (!current) return;
    setBusy(true);
    const ok = await save(current.name, draft);
    setBusy(false);
    setSaved(ok);
  };

  const doReset = async () => {
    if (!current) return;
    setBusy(true);
    await reset(current.name);
    setBusy(false);
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal prompt-modal" onClick={(e) => e.stopPropagation()}>
        <div className="panel-header">
          <h3>Prompt templates</h3>
          <button className="small" onClick={onClose}>
            Close
          </button>
        </div>

        {error && (
          <div className="error-banner" onClick={clearError} title="Click to dismiss">
            {error}
          </div>
        )}

        <div className="prompt-body">
          <ul className="prompt-list">
            {templates.map((t) => (
              <li
                key={t.name}
                className={t.name === selected ? "selected" : ""}
                onClick={() => setSelected(t.name)}
              >
                <span className="prompt-name">{t.name}</span>
                {t.modified && <span className="badge badge-warn">edited</span>}
              </li>
            ))}
            {templates.length === 0 && !loading && (
              <li className="empty">No templates in ~/.maestro/prompts</li>
            )}
          </ul>

          <div className="prompt-detail">
            {current ? (
              <>
                <p className="hint">
                  {current.description ?? "No description in frontmatter."}
                  {current.variables.length > 0 && (
                    <>
                      {" "}
                      Variables:{" "}
                      {current.variables.map((v) => (
                        <code key={v}>{`{{${v}}}`}</code>
                      ))}
                    </>
                  )}
                </p>
                <textarea
                  className="prompt-textarea"
                  spellCheck={false}
                  value={draft}
                  onChange={(e) => {
                    setDraft(e.target.value);
                    setSaved(false);
                  }}
                />
                <div className="prompt-actions">
                  <span className="session-meta">
                    {dirty ? "unsaved changes" : saved ? "saved" : ""}
                  </span>
                  <div className="actions">
                    {current.has_default && (
                      <button
                        className="small"
                        disabled={busy || !current.modified}
                        title={
                          current.modified
                            ? "Restore the built-in default"
                            : "Already identical to the default"
                        }
                        onClick={() => void doReset()}
                      >
                        Reset to default
                      </button>
                    )}
                    <button
                      className="small"
                      disabled={busy || !dirty}
                      onClick={() => void doSave()}
                    >
                      {busy ? "Saving…" : "Save"}
                    </button>
                  </div>
                </div>
              </>
            ) : (
              <p className="empty">{loading ? "Loading…" : "Select a template."}</p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
