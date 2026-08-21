import React from "react";
import { agents } from "../data/agents";
import "./ActivityFeed.css";

const statusLabel = {
  active: "Active",
  idle: "Idle",
  offline: "Offline",
};

function avatarColor(id) {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  const hue = hash % 360;
  return `hsl(${hue} 55% 45%)`;
}

function initials(name) {
  return name
    .split(/[-\s]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0].toUpperCase())
    .join("");
}

export default function ActivityFeed({ title = "Agent Activity" }) {
  return (
    <section className="activity-feed" aria-label={title}>
      <h2 className="activity-feed__title">{title}</h2>
      <ol className="activity-feed__list">
        {agents.map((agent) => {
          const state = agent.status in statusLabel ? agent.status : "idle";
          return (
            <li key={agent.id} className="activity-feed__item">
              <span
                className="activity-feed__avatar"
                style={{ backgroundColor: avatarColor(agent.id) }}
                aria-hidden="true"
              >
                {initials(agent.name)}
              </span>
              <div className="activity-feed__body">
                <div className="activity-feed__row">
                  <span className="activity-feed__agent">{agent.name}</span>
                  <span className="activity-feed__meta">
                    <span
                      className={`activity-feed__status-dot activity-feed__status-dot--${state}`}
                      aria-hidden="true"
                    />
                    <span className="activity-feed__status">
                      {statusLabel[state]}
                    </span>
                    <span className="activity-feed__last-seen">
                      &middot; {agent.lastSeen}
                    </span>
                  </span>
                </div>
                <p className="activity-feed__message">{agent.activity}</p>
              </div>
            </li>
          );
        })}
      </ol>
    </section>
  );
}