// A development-only preview: `DASHBOARD_FIXTURES=1 npm run dev` reads the snapshots `tests/agent_publish.rs`
// writes instead of the Blob store, and skips sign-in, so the screens can be designed and looked at locally.
// It is impossible in a production build: `next build` and `next start` set NODE_ENV=production.

export const FIXTURES = process.env.NODE_ENV !== "production" && process.env.DASHBOARD_FIXTURES === "1";
