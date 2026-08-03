// Mirrors the Rust gate types in src-tauri/src/core/gate/mod.rs.
// Keep the two in sync when the gate payloads change.

/** One user-editable value extracted from a gated command. */
export interface GateParam {
  key: string;
  label: string;
  value: string;
  multiline: boolean;
}

/** A tool call paused at the gate, waiting for the user's verdict. */
export interface PendingGate {
  gate_id: string;
  session_id: string;
  branch: string;
  kind: string;
  tool: string;
  params: GateParam[];
  raw_args: unknown;
}
