import { Component, type ReactNode } from "react";

/** A failed editor or lazy chunk must leave the repository browser usable. */
export class ViewBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <div className="session-error" role="alert">
        <h1>Unable to display this view</h1>
        <p>
          Open another file or reload this tab to try again. Conflict review choices are retained.
        </p>
        <button onClick={() => location.reload()}>Reload tab</button>
      </div>
    );
  }
}
