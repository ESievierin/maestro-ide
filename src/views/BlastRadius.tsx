import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useImpactReports } from "../state/impactReports";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { ImpactReport, ImpactedFile } from "../types/diffs";

/**
 * Blast radius of the branch's diff: which files outside it import or
 * reference what changed. Computed on demand in the core (bounded text scan
 * over tracked source files — no language server), rendered as two rings:
 * direct dependents, then files importing those. One click hands the whole
 * radius to the branch's main agent for verification.
 */
export function BlastRadius({
  branch,
  mergeBase,
  knownFiles,
}: {
  branch: string;
  /** The current snapshot's merge-base — part of the staleness signature. */
  mergeBase: string;
  knownFiles: string[];
}) {
  const cached = useImpactReports((s) => s.byBranch[branch]);
  const [report, setReport] = useState<ImpactReport | null>(cached?.report ?? null);
  const [signature, setSignature] = useState<string | null>(cached?.signature ?? null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [verifyBusy, setVerifyBusy] = useState(false);
  const [open, setOpen] = useState(true);

  const currentSignature = useMemo(
    () => `${mergeBase}:${[...knownFiles].sort().join("\n")}`,
    [mergeBase, knownFiles],
  );
  const stale = report !== null && signature !== null && signature !== currentSignature;

  const analyze = async () => {
    setBusy(true);
    setError(null);
    try {
      const fresh = await invoke<ImpactReport>("analyze_impact", { branch });
      setReport(fresh);
      setSignature(currentSignature);
      setOpen(true);
      useImpactReports.getState().set(branch, fresh, currentSignature);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const openFile = (path: string) => {
    void import("../utils/actions").then(({ openWorktree }) =>
      openWorktree(branch, "editor", path),
    );
  };

  /** Hand the radius to the main agent: verify each impacted file still holds. */
  const verify = async () => {
    if (!report || report.impacted.length === 0) return;
    setVerifyBusy(true);
    try {
      const direct = report.impacted.filter((f) => f.distance === 1);
      const indirect = report.impacted.filter((f) => f.distance === 2);
      const describe = (f: ImpactedFile) =>
        `- ${f.path} (${f.kind}: ${f.links.map((l) => `${l.matched} ← ${l.target}`).join(", ")})`;
      const prompt =
        `The diff on this branch changed:\n${report.analyzed.map((p) => `- ${p}`).join("\n")}\n\n` +
        `A blast-radius scan found these files referencing what changed:\n\n` +
        `Direct dependents:\n${direct.map(describe).join("\n") || "(none)"}\n` +
        (indirect.length > 0
          ? `\nSecond ring (import the direct dependents):\n${indirect.map(describe).join("\n")}\n`
          : "") +
        `\nGo through the dependents (read-only) and verify each one still holds against ` +
        `the change: call signatures, types/contracts, behavioral assumptions. Report ` +
        `anything that broke or looks risky, file by file — and say plainly when a file ` +
        `is unaffected. Don't modify anything.`;
      const main = await useSessions.getState().ensureMain(branch);
      if (!main) return;
      await useSessions.getState().send(main.id, prompt);
      useWorktrees.getState().setTab("chat");
    } finally {
      setVerifyBusy(false);
    }
  };

  const direct = report?.impacted.filter((f) => f.distance === 1) ?? [];
  const indirect = report?.impacted.filter((f) => f.distance === 2) ?? [];

  return (
    <div className="blast-radius">
      <div className="blast-radius-header">
        <button
          className="small ghost blast-radius-toggle"
          onClick={() => setOpen((o) => !o)}
          title="Which files outside this diff reference what changed"
        >
          <Icon name={open ? "chevron-down" : "chevron-right"} size={12} /> Blast radius
          {report && ` · ${report.impacted.length}`}
        </button>
        {stale && (
          <span
            className="review-guide-stale"
            title="The diff changed since this radius was computed — analyze again"
          >
            stale
          </span>
        )}
        <button
          className="small ghost"
          disabled={busy}
          onClick={() => void analyze()}
          title="Scan tracked source files for references to the changed ones"
        >
          {busy ? <Icon name="spinner" spin size={12} /> : <Icon name="refresh" size={12} />}
          {report ? "" : " Analyze"}
        </button>
      </div>

      {error && (
        <p className="hint warn" onClick={() => setError(null)} title="Click to dismiss">
          {error}
        </p>
      )}

      {open && report && (
        <>
          {report.impacted.length === 0 ? (
            <p className="blast-radius-empty">
              No outside references found ({report.scanned} files scanned).
            </p>
          ) : (
            <ul className="blast-radius-list">
              {direct.map((f) => (
                <ImpactRow key={f.path} file={f} onOpen={openFile} />
              ))}
              {indirect.length > 0 && (
                <li className="blast-radius-ring">second ring — imports the above</li>
              )}
              {indirect.map((f) => (
                <ImpactRow key={f.path} file={f} onOpen={openFile} />
              ))}
            </ul>
          )}
          {report.truncated && (
            <p className="blast-radius-empty">Capped — the radius may be wider.</p>
          )}
          {report.impacted.length > 0 && (
            <button
              className="small blast-radius-verify"
              disabled={verifyBusy}
              onClick={() => void verify()}
              title="Send the changed + impacted file list to the main agent to verify each dependent"
            >
              {verifyBusy ? <Icon name="spinner" spin size={12} /> : <Icon name="bot" size={12} />}{" "}
              Verify with main agent
            </button>
          )}
        </>
      )}
    </div>
  );
}

function ImpactRow({ file, onOpen }: { file: ImpactedFile; onOpen: (path: string) => void }) {
  const tooltip = file.links.map((l) => `${l.kind} of ${l.matched} (from ${l.target})`).join("\n");
  const name = file.path.split("/").pop() ?? file.path;
  const dir = file.path.slice(0, file.path.length - name.length);
  return (
    <li
      className="blast-radius-row"
      title={`${file.path}\n${tooltip}\n\nClick to open in the editor`}
      onClick={() => onOpen(file.path)}
    >
      <span className={`blast-radius-kind ${file.kind}`}>
        {file.kind === "import" ? "imp" : "ref"}
      </span>
      <span className="blast-radius-path">
        <span className="blast-radius-dir">{dir}</span>
        {name}
      </span>
    </li>
  );
}
