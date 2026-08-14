import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "./Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useEventLog } from "../state/events";

/**
 * Raw bus events, for debugging. Hidden by default behind the header's icon button —
 * this is a developer tool, not something every user needs staring at them all the time.
 */
export function EventLog({ onClose }: { onClose: () => void }) {
  const events = useEventLog((s) => s.events);
  const clear = useEventLog((s) => s.clear);
  const [filter, setFilter] = useState("");
  useEscapeToClose(onClose);

  const emitTestEvent = () => {
    void invoke("emit_test_event", { message: "hello from the frontend" });
  };

  // Substring match over the type and the payload — a busy fleet buries the
  // one event you care about within seconds.
  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return events;
    return events.filter(
      (e) =>
        e.event.type.toLowerCase().includes(q) ||
        JSON.stringify(e.event.data).toLowerCase().includes(q),
    );
  }, [events, filter]);

  return (
    <section className="event-drawer">
      <div className="panel-header">
        <h2>
          Event log{" "}
          <span className="count">
            ({filter.trim() ? `${shown.length}/${events.length}` : events.length})
          </span>
        </h2>
        <div className="actions">
          <input
            type="text"
            className="event-filter"
            placeholder="Filter by type or payload…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && filter) {
                e.stopPropagation();
                setFilter("");
              }
            }}
          />
          <button className="small ghost" onClick={emitTestEvent}>
            Emit test event
          </button>
          <button className="small ghost" onClick={clear}>
            Clear
          </button>
          <button className="small icon-only ghost" onClick={onClose} title="Close">
            <Icon name="close" />
          </button>
        </div>
      </div>
      <div className="event-log-body">
        {events.length === 0 ? (
          <p className="empty">No events yet. Core events will appear here.</p>
        ) : shown.length === 0 ? (
          <p className="empty">No events match "{filter.trim()}".</p>
        ) : (
          <ul>
            {shown.map((e, i) => (
              <li key={i}>
                <span className="event-type">{e.event.type}</span>
                <code>{JSON.stringify(e.event.data)}</code>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
