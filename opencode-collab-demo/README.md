# Agent Relay

A small polished React + Vite dashboard demonstrating live two-machine agent
collaboration over FeanorFS encrypted signals.

## What it is

Agent Relay renders an activity timeline of cross-machine coding agents built
as a real, boringly-practical coordination job:

- **mac-opencode** (lead) authored the Vite scaffold: `package.json`,
  `vite.config.js`, `index.html`, `src/main.jsx`, `src/styles.css`,
  `src/App.jsx`, and this file.
- **cachyos-opencode** authored the `ActivityFeed` component and its agent
  data under `src/components/` and `src/data/`.

The two agents coordinated through FeanorFS `ffwork1` work intents with the
human as coordinator, keeping file scopes disjoint and exchanging the
component over encrypted signal snapshots.

## Run it

```bash
npm install
npm run dev
```

`npm run build` produces a production bundle under `dist/`.

## Scope

This demo intentionally ships no dependencies into the FeanorFS workspace —
`node_modules` is never created here.

## Contract

`ActivityFeed` is the default export, renders a compact accessible timeline
from an exported `agents` array, and uses only React.