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
  useEscapeToClose(onClose);

  const emitTestEvent = () => {
    void invoke("emit_test_event", { message: "hello from the frontend" });
  };

  return (
    <section className="event-drawer">
      <div className="panel-header">
        <h2>
          Event log <span className="count">({events.length})</span>
        </h2>
        <div className="actions">
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
    </section>
  );
}
