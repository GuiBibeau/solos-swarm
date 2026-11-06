import { startLogin } from "@/lib/auth-flow";
import { configOrNull, notConfigured } from "@/lib/auth-routes";

export const dynamic = "force-dynamic";

export async function GET(request: Request): Promise<Response> {
  const config = configOrNull();
  return config ? startLogin(request, { config }) : notConfigured();
}
