import { JOB_LABEL, STAGE_LABEL, jobTone, stageTone } from "@/lib/format";

export function StageBadge({ stage, outcome }: { stage: string; outcome?: string }) {
  const label = stage === "ended" && outcome ? outcome : (STAGE_LABEL[stage] ?? stage);
  return <span className={`badge ${stageTone(stage, outcome)}`}>{label}</span>;
}

export function JobBadge({ status }: { status: string }) {
  return <span className={`badge ${jobTone(status)}`}>{JOB_LABEL[status] ?? status}</span>;
}
