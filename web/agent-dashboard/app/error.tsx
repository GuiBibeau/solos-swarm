"use client";

export default function ErrorPage({ reset }: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <>
      <h1>Something went wrong</h1>
      <p className="muted">The page could not be built. It is logged on the server.</p>
      <button type="button" className="link" onClick={reset}>
        Try again
      </button>
    </>
  );
}
