import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { SelectMenu, type SelectMenuOption } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { askViaNewSession, findResumableSession } from "../utils/agentAsk";
import { useSessions } from "../state/sessions";
import { usePr, openUrl, type CreatedPr } from "../state/pr";
import { useWorktrees } from "../state/worktrees";
import type { RepoInfo, WorktreeInfo } from "../types/worktrees";

/** "TITLE: ...\n\nbody" → { title, body }; a missing TITLE: line falls back
 * to the first line as the title. Mirrors the backend's own parser. */
function parsePrDraft(raw: string): { title: string; body: string } {
  const trimmed = raw.trim();
  const rest = trimmed.startsWith("TITLE:") ? trimmed.slice("TITLE:".length) : trimmed;
  const nl = rest.indexOf("\n");
  if (nl === -1) return { title: rest.trim(), body: "" };
  return { title: rest.slice(0, nl).trim(), body: rest.slice(nl + 1).trim() };
}

const GEN_MODEL = "sonnet";
const GEN_EFFORT = "high";

/**
 * One button from "pile of changes" to "PR is up": pick the base, a commit
 * message on top (typed or generated), PR title seeded from it, description
 * generated from the branch, then commit → push → `gh pr create`.
 *
 * Generation asks a real agent, not a stateless CLI call: it resumes the
 * branch's own implementation session when one exists, so the answer
 * reflects what that agent actually did and why — not just a diff read
 * cold. The prompts themselves stay the editable `commit-message` /
 * `pr-description` templates.
 */
export function CreatePrDialog({
  worktree,
  repo,
  onClose,
}: {
  worktree: WorktreeInfo;
  repo: RepoInfo | null;
  onClose: () => void;
}) {
  const branch = worktree.branch as string;
  const dirty = worktree.status?.dirty ?? false;
  const renderCommitPrompt = usePr((s) => s.renderCommitPrompt);
  const renderPrPrompt = usePr((s) => s.renderPrPrompt);
  const createPr = usePr((s) => s.createPr);
  useEscapeToClose(onClose);

  const baseOptions = useMemo<SelectMenuOption[]>(() => {
    const seen = new Set<string>();
    const options: SelectMenuOption[] = [];
    for (const b of repo?.branches ?? []) {
      if (b === branch || seen.has(b)) continue;
      seen.add(b);
      options.push({ value: b, label: b });
    }
    for (const remote of repo?.remote_branches ?? []) {
      const short = remote.slice(remote.indexOf("/") + 1);
      if (!short || short === branch || seen.has(short)) continue;
      seen.add(short);
      options.push({ value: short, label: short, description: `from ${remote}` });
    }
    return options;
  }, [repo, branch]);

  const [base, setBase] = useState<string>(() => {
    const stored = worktree.base_branch;
    const short = stored && stored.includes("/") ? stored.slice(stored.indexOf("/") + 1) : stored;
    for (const candidate of [stored, short]) {
      if (candidate && baseOptions.some((o) => o.value === candidate)) return candidate;
    }
    if (repo?.default_branch && baseOptions.some((o) => o.value === repo.default_branch)) {
      return repo.default_branch;
    }
    return baseOptions[0]?.value ?? "";
  });

  const [commitMessage, setCommitMessage] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [genCommitBusy, setGenCommitBusy] = useState(false);
  const [genPrBusy, setGenPrBusy] = useState(false);
  const [submitBusy, setSubmitBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [result, setResult] = useState<CreatedPr | null>(null);

  const needCommit = dirty;
  const canSubmit =
    !submitBusy &&
    base.length > 0 &&
    title.trim().length > 0 &&
    (!needCommit || commitMessage.trim().length > 0);

  /** The branch's own implementation session, if one is resumable — the
   * generated text is only as good as the context it can see. */
  const contextSession = async () => {
    await useSessions.getState().fetch(branch);
    return findResumableSession(branch, ["implementation"]);
  };

  const genCommit = async () => {
    setGenCommitBusy(true);
    try {
      const prompt = await renderCommitPrompt(branch, base || null);
      if (!prompt) return;
      const target = await contextSession();
      const result = await askViaNewSession({
        branch,
        prompt,
        resumeFrom: target?.id,
        model: GEN_MODEL,
        effort: GEN_EFFORT,
        permissionMode: "plan",
      });
      if (result?.text) {
        setCommitMessage(result.text);
        if (!title.trim()) setTitle(result.text.split("\n")[0] ?? "");
      }
    } finally {
      setGenCommitBusy(false);
    }
  };

  const genPr = async () => {
    setGenPrBusy(true);
    try {
      const rendered = await renderPrPrompt(branch, base || null);
      if (!rendered) return;
      setBase(rendered.base);
      const target = await contextSession();
      const result = await askViaNewSession({
        branch,
        prompt: rendered.prompt,
        resumeFrom: target?.id,
        model: GEN_MODEL,
        effort: GEN_EFFORT,
        permissionMode: "plan",
      });
      if (result?.text) {
        const draft = parsePrDraft(result.text);
        if (draft.title) setTitle(draft.title);
        setBody(draft.body);
      }
    } finally {
      setGenPrBusy(false);
    }
  };

  const submit = async () => {
    setSubmitBusy(true);
    try {
      if (needCommit) {
        setPhase("Committing…");
        try {
          await invoke<string>("commit_worktree", { branch, message: commitMessage.trim() });
        } catch {
          return; // error toast already up; keep the dialog as-is
        }
      }
      setPhase("Pushing & creating the PR…");
      const created = await createPr(branch, title.trim(), body, base || null);
      if (created) {
        setResult(created);
        void useWorktrees.getState().refresh();
      }
    } finally {
      setSubmitBusy(false);
      setPhase(null);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal createpr-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="pr" /> Create PR · {branch}
        </h3>

        {result ? (
          <>
            <p className="hint success">
              <Icon name="check" /> Pull request created:{" "}
              <button className="small" onClick={() => void openUrl(result.url)}>
                <Icon name="external-link" /> {result.url}
              </button>
            </p>
            {result.push_report && <pre className="check-output">{result.push_report}</pre>}
          </>
        ) : (
          <div className="form-grid">
            <label>
              Base branch
              <SelectMenu
                value={base}
                onChange={setBase}
                options={baseOptions}
                placeholder={baseOptions.length === 0 ? "no other branch found" : undefined}
              />
            </label>

            {needCommit && (
              <label>
                Commit message — uncommitted changes will be committed first
                <div className="gen-row">
                  <textarea
                    rows={3}
                    placeholder="What and why — or let it be written from the diff"
                    value={commitMessage}
                    onChange={(e) => {
                      setCommitMessage(e.target.value);
                      if (!title.trim()) setTitle(e.target.value.split("\n")[0] ?? "");
                    }}
                  />
                  <button
                    className="small ghost"
                    disabled={genCommitBusy}
                    title="Ask the branch's agent to write it, with full context of what it did"
                    onClick={() => void genCommit()}
                  >
                    {genCommitBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate
                  </button>
                </div>
              </label>
            )}

            <label>
              PR title
              <input
                type="text"
                placeholder={needCommit ? "Defaults to the commit summary" : "Imperative, short"}
                value={title}
                onChange={(e) => setTitle(e.target.value)}
              />
            </label>

            <label>
              Description
              <div className="gen-row">
                <textarea
                  rows={8}
                  placeholder="What changed, why, review notes — or generate it from the branch"
                  value={body}
                  onChange={(e) => setBody(e.target.value)}
                />
                <button
                  className="small ghost"
                  disabled={genPrBusy}
                  title="Ask the branch's agent — includes uncommitted changes, not just what's committed"
                  onClick={() => void genPr()}
                >
                  {genPrBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate
                </button>
              </div>
            </label>

            {phase && (
              <p className="hint">
                <Icon name="spinner" spin /> {phase}
              </p>
            )}
          </div>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            {result ? "Close" : "Cancel"}
          </button>
          {!result && (
            <button className="btn-primary" disabled={!canSubmit} onClick={() => void submit()}>
              {submitBusy
                ? "Working…"
                : needCommit
                  ? "Commit, push & create PR"
                  : "Push & create PR"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
