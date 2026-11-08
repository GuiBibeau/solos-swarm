import Link from "next/link";
import { Mark } from "@/components/rail";

export default function NotFound() {
  return (
    <div className="standalone">
      <span className="standalone-mark" aria-hidden="true">
        <Mark />
      </span>
      <h1>Nothing here</h1>
      <p className="sub">That address is not part of the command center.</p>
      <Link className="button primary" href="/">
        Back to the chat
      </Link>
    </div>
  );
}
