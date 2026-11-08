import { redirect } from "next/navigation";

/** The chat is the home now; an old link lands on it. */
export default function ChatRedirect(): never {
  redirect("/");
}
