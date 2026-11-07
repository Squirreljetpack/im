import { applyD1Migrations } from "cloudflare:test";
import { env } from "cloudflare:workers";

// The `TEST_MIGRATIONS` binding is injected by vitest.config.ts.
await applyD1Migrations(env.AUTH_DB, env.TEST_MIGRATIONS);
