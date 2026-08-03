import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useEventLog } from "../state/events";

export function EventLog() {
  const events = useEventLog((s) => s.events);
  const clear = useEventLog((s) => s.clear);
  const [open, setOpen] = useState(false);

  const emitTestEvent = () => {
    void invoke("emit_test_event", { message: "hello from the frontend" });
  };

  return (
    <section className={`event-log ${open ? "open" : "closed"}`}>
      <div className="panel-header clickable" onClick={() => setOpen(!open)}>
        <h2>
          Event log <span className="count">({events.length})</span>
        </h2>
        <div className="actions" onClick={(e) => e.stopPropagation()}>
          <button className="small" onClick={emitTestEvent}>
            Emit test event
          </button>
          <button className="small" onClick={clear}>
            Clear
          </button>
        </div>
      </div>
      {open && (
        <div className="event-log-body">
          {events.length === 0 ? (
            <p className="empty">No events yet. Core events will appear here.</p>
          ) : (
            <ul>
              {events.map((e, i) => (
                <li key={i}>
                  <span className="event-type">{e.event.type}</span>
                  <code>{JSON.stringify(e.event.data)}</code>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}
