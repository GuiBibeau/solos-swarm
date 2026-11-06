import { signOut } from "@/lib/auth-flow";
import { configOrNull, notConfigured } from "@/lib/auth-routes";

export const dynamic = "force-dynamic";

// POST only: a cross-site form cannot carry the SameSite=Lax session cookie, and a link or an image cannot sign
// anyone out.
export async function POST(): Promise<Response> {
  const config = configOrNull();
  return config ? signOut(config) : notConfigured();
}
