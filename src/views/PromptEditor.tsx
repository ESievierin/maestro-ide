import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { usePrompts } from "../state/prompts";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/**
 * Prompt templates live as markdown files in `~/.maestro/prompts`; this is a plain
 * editor over them. Saving takes effect on the next render in the core — no restart.
 */
export function PromptEditor({ onClose }: { onClose: () => void }) {
  const {
    templates,
    loading,
    error,
    fetch,
    save,
    reset,
    delete: deleteTemplate,
    clearError,
  } = usePrompts();
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [previewOn, setPreviewOn] = useState(false);
  const [preview, setPreview] = useState("");
  useEscapeToClose(onClose);

  // Preview always reflects the current draft (saved or not); recomputed on
  // toggle and on edits made while the preview is showing.
  useEffect(() => {
    if (!previewOn) return;
    void invoke<string>("preview_prompt", { content: draft })
      .then(setPreview)
      .catch((e) => setPreview(String(e)));
  }, [previewOn, draft]);

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

  const doDelete = async () => {
    if (!current) return;
    const { confirm } = await import("@tauri-apps/plugin-dialog");
    const ok = await confirm(`Delete the "${current.name}" template? This cannot be undone.`, {
      title: "MaestroIDE",
      kind: "warning",
    });
    if (!ok) return;
    setBusy(true);
    const deleted = await deleteTemplate(current.name);
    setBusy(false);
    if (deleted) setSelected(null);
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal prompt-modal" onClick={(e) => e.stopPropagation()}>
        <div className="panel-header">
          <h3>
            <Icon name="file-text" /> Prompt templates
          </h3>
          <button className="small icon-only ghost" onClick={onClose} title="Close">
            <Icon name="close" />
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
                {previewOn ? (
                  <pre
                    className="prompt-textarea prompt-preview"
                    title="Rendered with sample values for each declared variable"
                  >
                    {preview}
                  </pre>
                ) : (
                  <textarea
                    className="prompt-textarea"
                    spellCheck={false}
                    value={draft}
                    onChange={(e) => {
                      setDraft(e.target.value);
                      setSaved(false);
                    }}
                  />
                )}
                <div className="prompt-actions">
                  <span className="session-meta">
                    {dirty ? "unsaved changes" : saved ? "saved" : ""}
                  </span>
                  <div className="actions">
                    <button
                      className={`small ${previewOn ? "" : "ghost"}`}
                      title="Render the draft with sample values for each declared variable"
                      onClick={() => setPreviewOn((p) => !p)}
                    >
                      <Icon name="file-text" size={12} /> {previewOn ? "Edit" : "Preview"}
                    </button>
                    {!current.has_default && (
                      <button
                        className="small danger"
                        disabled={busy}
                        title="Delete this custom template"
                        onClick={() => void doDelete()}
                      >
                        <Icon name="trash" size={12} /> Delete
                      </button>
                    )}
                    {current.update_available && (
                      <span
                        className="badge badge-warn"
                        title="The built-in default changed since your edit — resetting picks up the newer default (and discards the edit)"
                      >
                        default updated
                      </span>
                    )}
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
                      className="small btn-primary"
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
