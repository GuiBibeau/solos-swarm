import { redirect } from "next/navigation";
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
  if (await currentViewer()) redirect("/");
  const { error, next } = await searchParams;
  const message = error ? (MESSAGES[error] ?? "The sign-in failed. Try again.") : null;
  const target = next && next.startsWith("/") && !next.startsWith("//") ? next : "/";
  return (
    <div className="signin">
      <h1>Agent</h1>
      <p className="muted">What the agent is doing, and what it said. Private.</p>
      {message ? (
        <p className="error" role="alert">
          {message}
        </p>
      ) : null}
      <a className="button" href={`/auth/login?next=${encodeURIComponent(target)}`}>
        Sign in with Vercel
      </a>
    </div>
  );
}
