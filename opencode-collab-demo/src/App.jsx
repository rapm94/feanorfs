import ActivityFeed from "./components/ActivityFeed.jsx";

const relayStats = [
  { label: "Active agents", value: "3" },
  { label: "Messages relayed", value: "1,284" },
  { label: "Conflicts resolved", value: "7" },
  { label: "Uptime", value: "99.98%" },
];

function App() {
  return (
    <div className="app">
      <header className="app-header">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true"></span>
          <h1>Agent Relay</h1>
        </div>
        <p className="tagline">
          A live two-machine activity dashboard built over FeanorFS encrypted
          signals.
        </p>
      </header>

      <main className="app-main">
        <section className="stats" aria-label="Relay stats">
          {relayStats.map((stat) => (
            <div className="stat-card" key={stat.label}>
              <span className="stat-value">{stat.value}</span>
              <span className="stat-label">{stat.label}</span>
            </div>
          ))}
        </section>

        <ActivityFeed />
      </main>

      <footer className="app-footer">
        <p>Agent Relay demo — mac-opencode lead, cachyos-opencode component.</p>
      </footer>
    </div>
  );
}

export default App;