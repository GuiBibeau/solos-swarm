import Link from "next/link";

export default function NotFound() {
  return (
    <>
      <h1>Not found</h1>
      <p className="muted">There is nothing at this address.</p>
      <Link href="/">Back to the tickets</Link>
    </>
  );
}
