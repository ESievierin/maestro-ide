import type { Session, ToolChild, TranscriptItem } from "../types/sessions";

function renderChild(child: ToolChild): string {
  switch (child.kind) {
    case "text":
      return `  ${child.text.replace(/\n/g, "\n  ")}`;
    case "thinking":
      return `  *Thinking:* ${child.text.replace(/\n/g, "\n  ")}`;
    case "tool_use":
      return `  \`${child.name}\`: ${child.summary}`;
  }
}

function renderItem(item: TranscriptItem): string {
  switch (item.kind) {
    case "user":
      return `**You:**\n\n${item.text}`;
    case "text":
      return `**Claude:**\n\n${item.text}`;
    case "thinking":
      return `**Thinking:**\n\n${item.text}`;
    case "tool_use": {
      const blocks = [`**Tool: \`${item.name}\`**\n\n${item.summary}`];
      if (item.children.length > 0) {
        blocks.push(item.children.map(renderChild).join("\n"));
      }
      if (item.result) {
        const label = item.result.isError ? "Error" : "Result";
        blocks.push(`${label}:\n\n\`\`\`\n${item.result.text}\n\`\`\``);
      }
      return blocks.join("\n\n");
    }
    case "denied":
      return `**Denied — ${item.tool}:** ${item.message}`;
    case "permission_request":
      return `**Permission requested — ${item.tool}${item.title ? `: ${item.title}` : ""}** (${item.resolved})`;
    case "status":
      return `*— ${item.status.replace(/_/g, " ")} —*`;
    case "dialog":
      return [`**${item.title}**`, ...item.lines.map((l) => `- ${l}`)].join("\n");
    case "settings":
      return `*Settings changed: ${item.text}*`;
  }
}

/** Render a session's transcript as a plain markdown document — the same
 * information the chat view shows, in a form worth sharing outside the app
 * or archiving. */
export function transcriptToMarkdown(session: Session, items: TranscriptItem[]): string {
  const header = [
    `# Session transcript — ${session.branch} (${session.session_type})`,
    "",
    `- Model: ${session.model ?? "default"}`,
    `- Effort: ${session.effort ?? "default"}`,
    `- Started: ${session.created_at}`,
    `- Exported: ${new Date().toISOString()}`,
    "",
    "---",
    "",
  ].join("\n");

  const body = items.map(renderItem).join("\n\n---\n\n");
  return `${header}${body}\n`;
}

/** A filesystem-safe default filename: no path separators or reserved
 * Windows characters, and short enough to read at a glance in a save dialog. */
export function defaultTranscriptFilename(session: Session): string {
  const branchSlug = session.branch.replace(/[\\/:*?"<>|]+/g, "-");
  return `transcript-${branchSlug}-${session.id.slice(0, 8)}.md`;
}
