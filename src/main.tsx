import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { startEventBridge } from "./state/events";
import "./styles.css";

// Subscribe to backend events before the first render so no early event is missed.
startEventBridge();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
