import { redirect } from "next/navigation";
import { Field } from "@/components/field";
import { Mark } from "@/components/rail";
import { FIXTURES } from "@/lib/fixtures";
import { currentViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Sign in" };

// A message per code, so what a URL says is never what the page prints.
const MESSAGES: Record<string, string> = {
  state: "That sign-in did not start here, or took too long. Try again.",
  provider: "Vercel did not complete the sign-in. Try again.",
  token: "Vercel would not confirm the sign-in. Try again in a moment.",
  identity: "The sign-in could not be verified. Try again.",
  forbidden: "That Vercel account is not allowed here.",
  config: "The dashboard is not configured.",
};

export default async function Login({
  searchParams,
}: {
  searchParams: Promise<{ error?: string; next?: string }>;
}) {
  if (!FIXTURES && (await currentViewer())) redirect("/");
  const { error, next } = await searchParams;
  const message = error ? (MESSAGES[error] ?? "The sign-in failed. Try again.") : null;
  const target = next && next.startsWith("/") && !next.startsWith("//") ? next : "/";
  return (
    <div className="signin">
      <div className="signin-copy">
        <span className="signin-mark" aria-hidden="true">
          <Mark />
        </span>
        <h1>The swarm, in one place.</h1>
        <p className="sub">
          solos agent: what it is working on, what it needs from you, and what it said. Private to you.
        </p>
        {message ? (
          <p className="notice signin-notice" role="alert">
            {message}
          </p>
        ) : null}
        <a className="button primary signin-action" href={`/auth/login?next=${encodeURIComponent(target)}`}>
          Sign in with Vercel
        </a>
        <p className="signin-note">One allowed Vercel account. The box never accepts a connection.</p>
      </div>
      <div className="signin-field" aria-hidden="true">
        <Field tickets={[]} large />
      </div>
    </div>
  );
}
