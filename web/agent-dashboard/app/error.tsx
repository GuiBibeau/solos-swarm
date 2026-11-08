"use client";

export default function ErrorPage({ reset }: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <div className="standalone">
      <h1>The page did not build</h1>
      <p className="sub">Something on the server failed. It is logged there; trying again is safe.</p>
      <button type="button" className="button primary" onClick={reset}>
        Try again
      </button>
    </div>
  );
}
