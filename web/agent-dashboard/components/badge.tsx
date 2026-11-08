import { JOB_LABEL, STAGE_LABEL, jobTone, stageTone } from "@/lib/format";

const TONE: Record<string, string> = {
  attention: "warn",
  bad: "bad",
  live: "live",
  good: "good",
  quiet: "",
};

export function StageBadge({ stage, outcome }: { stage: string; outcome?: string }) {
  const label = stage === "ended" && outcome ? outcome : (STAGE_LABEL[stage] ?? stage);
  return <span className={`tag ${TONE[stageTone(stage, outcome)] ?? ""}`}>{label}</span>;
}

export function JobBadge({ status }: { status: string }) {
  return <span className={`tag ${TONE[jobTone(status)] ?? ""}`}>{JOB_LABEL[status] ?? status}</span>;
}
