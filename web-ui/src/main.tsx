import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { ThemeProvider } from "./theme";
import { ViewBoundary } from "./ViewBoundary";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ThemeProvider>
      <ViewBoundary>
        <App />
      </ViewBoundary>
    </ThemeProvider>
  </StrictMode>,
);
