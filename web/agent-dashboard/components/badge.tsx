import { STAGE_LABEL, stageTone } from "@/lib/format";

export function StageBadge({ stage, outcome }: { stage: string; outcome?: string }) {
  const label = stage === "ended" && outcome ? outcome : (STAGE_LABEL[stage] ?? stage);
  return <span className={`badge ${stageTone(stage, outcome)}`}>{label}</span>;
}
