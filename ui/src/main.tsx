import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource/open-sans/400.css";
import "@fontsource/open-sans/500.css";
import "@fontsource/open-sans/600.css";
import "./index.css";
import "./app.css";
import App from "./App";
import { DialogsProvider } from "./components/Dialogs";
import { SettingsProvider, DEFAULT_SETTINGS } from "./settings";
import { api } from "./api";

// Settings are fetched once, before the first render, so every
// consumer (ChatView, useSessions, ...) can read them synchronously
// off context instead of threading a loading state everywhere.
api
  .getSettings()
  .catch(() => DEFAULT_SETTINGS)
  .then((settings) => {
    createRoot(document.getElementById("root")!).render(
      <StrictMode>
        <SettingsProvider initial={settings}>
          <DialogsProvider>
            <App />
          </DialogsProvider>
        </SettingsProvider>
      </StrictMode>,
    );
  });
