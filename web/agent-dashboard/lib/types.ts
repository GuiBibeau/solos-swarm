// The shapes `solos-swarm agent publish` uploads: src/agent/publish/snapshot.rs is the other side of this file,
// and its FORMAT constant is the `v` below. Optional fields are the ones Rust leaves out when it has nothing to say.

export const FORMAT = 1;

export type Stage =
  | "approval"
  | "answer"
  | "blocked"
  | "failed"
  | "queued"
  | "idle"
  | "working"
  | "ended";

export type Person = {
  id: string;
  kind: "person" | "bot" | "system";
  name?: string;
};

export type AssignmentBrief = {
  state: "pending" | "running" | "ended";
  report?: string;
  reports: number;
};

export type IndexRow = {
  id: string;
  title: string;
  stage: Stage;
  open: boolean;
  outcome?: "completed" | "dropped" | "stopped";
  created_ms: number;
  created_at: string;
  updated_ms: number;
  updated_at: string;
  duration_ms?: number;
  source: string;
  asker: Person;
  spent_microusd: number;
  budget_microusd?: number;
  assignment?: AssignmentBrief;
  waiting_for?: string;
};

export type Index = {
  v: number;
  generated_at?: string;
  open: number;
  tickets: IndexRow[];
};

export type Delivery = {
  delivered: boolean;
  attempts: number;
  error?: string;
};

export type Turn = {
  at_ms: number;
  at: string;
  from: "person" | "bot" | "system" | "router" | "agent";
  who?: string;
  kind:
    | "request"
    | "message"
    | "acknowledgement"
    | "result"
    | "attention"
    | "note"
    | "approval"
    | "secret";
  text: string;
  truncated?: boolean;
  channel?: string;
  delivery?: Delivery;
};

export type DecisionView = {
  id: string;
  kind: "accept" | "ignore" | "stop" | "unavailable";
  reason: string;
  at_ms: number;
  at: string;
};

export type ReportView = {
  seq: number;
  status: string;
  text: string;
  truncated?: boolean;
  question?: string;
  spent_microusd: number;
  at_ms: number;
  at: string;
};

export type AssignmentView = {
  id: string;
  state: "pending" | "running" | "ended";
  created_ms: number;
  created_at: string;
  started_ms?: number;
  ended_ms?: number;
  ended_reason?: string;
  report_count: number;
  reports: ReportView[];
};

export type PageView = {
  n: number;
  at: string;
  action: string;
  url: string;
  title: string;
  status?: number;
};

export type BrowserView = {
  steps: number;
  refused: number;
  lost: number;
  closed: number;
  needs_approval: string[];
  pages: PageView[];
  pages_omitted: number;
};

export type TicketSnapshot = {
  v: number;
  id: string;
  title: string;
  stage: Stage;
  open: boolean;
  created_ms: number;
  created_at: string;
  updated_ms: number;
  updated_at: string;
  ended?: {
    at_ms: number;
    at: string;
    by: "person" | "executor" | "policy";
    outcome: "completed" | "dropped" | "stopped";
    reason: string;
  };
  scope: string;
  plan: {
    kind: "session" | "swarm";
    harness?: string;
    environment?: string;
    agents?: number;
  };
  repo?: string;
  deliverable: "answer" | "change" | "pull_request";
  approval: { state: "not_needed" | "waiting" | "granted"; by?: string; at?: string };
  budget_microusd: number | null;
  spent_microusd: number;
  attention?: string;
  source: string;
  source_url?: string;
  requester: Person;
  request?: Turn;
  conversation: Turn[];
  decisions: DecisionView[];
  assignments: AssignmentView[];
  browser?: BrowserView;
};
