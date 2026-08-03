// CodeMirror 6 extension for T6: track the current line selection on the "new" side
// of a diff editor, and render line-question answers as block widgets between lines.

import type { EditorState } from "@codemirror/state";
import { StateEffect, StateField } from "@codemirror/state";
import { Decoration, type DecorationSet, EditorView, WidgetType } from "@codemirror/view";
import type { LineQuestion } from "../types/questions";

/** A 1-based, inclusive line range. */
export interface LineRange {
  start: number;
  end: number;
}

/** Dispatched whenever the set of tracked questions for the visible file changes. */
export const setLineQuestions = StateEffect.define<readonly LineQuestion[]>();

class LineQuestionWidget extends WidgetType {
  constructor(private readonly question: LineQuestion) {
    super();
  }

  eq(other: LineQuestionWidget): boolean {
    return (
      this.question.id === other.question.id &&
      this.question.status === other.question.status &&
      this.question.answer === other.question.answer
    );
  }

  toDOM(): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "line-question-block";

    const question = document.createElement("div");
    question.className = "line-question-text";
    question.textContent = `Q: ${this.question.question}`;
    wrap.appendChild(question);

    const answer = document.createElement("div");
    answer.className = "line-question-answer";
    answer.textContent =
      this.question.status === "waiting" ? "Waiting for answer…" : this.question.answer || "…";
    wrap.appendChild(answer);

    return wrap;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

function buildDecorations(state: EditorState, questions: readonly LineQuestion[]): DecorationSet {
  const widgets = questions
    .filter((q) => q.lineEnd >= 1 && q.lineEnd <= state.doc.lines)
    .map((q) =>
      Decoration.widget({ widget: new LineQuestionWidget(q), side: 1, block: true }).range(
        state.doc.line(q.lineEnd).to,
      ),
    );
  return Decoration.set(widgets, true);
}

/** Renders line-question answer blocks below their target line. Rebuilds decorations
 * on `setLineQuestions`; never recreates the editor, so scroll position is preserved. */
export const lineQuestionsField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const effect of tr.effects) {
      if (effect.is(setLineQuestions)) {
        return buildDecorations(tr.state, effect.value);
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
  provide: (field) => EditorView.decorations.from(field),
});

/** Reports the 1-based line range of the current selection, or `null` for a plain
 * caret (no range selected). */
export function selectionListener(onChange: (range: LineRange | null) => void) {
  return EditorView.updateListener.of((update) => {
    if (!update.selectionSet && !update.docChanged) return;
    const sel = update.state.selection.main;
    if (sel.empty) {
      onChange(null);
      return;
    }
    onChange({
      start: update.state.doc.lineAt(sel.from).number,
      end: update.state.doc.lineAt(sel.to).number,
    });
  });
}
