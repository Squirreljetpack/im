/**
 * Accounts: PBKDF2-SHA256 password hashing and HS256 bearer tokens.
 *
 * Both use WebCrypto so the Worker needs no native dependencies; the JWT is
 * signed with the `JWT_SECRET` binding (`wrangler secret put JWT_SECRET`).
 */

import { SignJWT, jwtVerify } from "jose";

const encoder = new TextEncoder();
const PBKDF2_ITERATIONS = 100_000;
const TOKEN_TTL_SECONDS = 60 * 60 * 24 * 30;

export function toBase64(bytes: Uint8Array): string {
  let text = "";
  for (const byte of bytes) {
    text += String.fromCharCode(byte);
  }
  return btoa(text);
}

export function fromBase64(text: string): Uint8Array {
  const raw = atob(text);
  const bytes = new Uint8Array(raw.length);
  for (let index = 0; index < raw.length; index += 1) {
    bytes[index] = raw.charCodeAt(index);
  }
  return bytes;
}

async function derive(
  password: string,
  salt: Uint8Array,
  iterations: number,
): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(password),
    "PBKDF2",
    false,
    ["deriveBits"],
  );
  const bits = await crypto.subtle.deriveBits(
    { name: "PBKDF2", hash: "SHA-256", salt, iterations },
    key,
    256,
  );
  return new Uint8Array(bits);
}

/** `pbkdf2$sha256$<iterations>$<salt>$<hash>` (both base64). */
export async function hashPassword(password: string): Promise<string> {
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const hash = await derive(password, salt, PBKDF2_ITERATIONS);
  return [
    "pbkdf2",
    "sha256",
    String(PBKDF2_ITERATIONS),
    toBase64(salt),
    toBase64(hash),
  ].join("$");
}

export async function verifyPassword(
  password: string,
  stored: string,
): Promise<boolean> {
  const [scheme, digest, iterations, salt, expected] = stored.split("$");
  if (scheme !== "pbkdf2" || digest !== "sha256" || !salt || !expected) {
    return false;
  }
  const rounds = Number(iterations);
  if (!Number.isInteger(rounds) || rounds <= 0) {
    return false;
  }
  const actual = await derive(password, fromBase64(salt), rounds);
  const expectedBytes = fromBase64(expected);
  if (actual.length !== expectedBytes.length) {
    return false;
  }
  return crypto.subtle.timingSafeEqual(actual, expectedBytes);
}

export async function issueToken(
  userId: string,
  secret: string,
  now: number,
): Promise<string> {
  return new SignJWT({})
    .setProtectedHeader({ alg: "HS256" })
    .setSubject(userId)
    .setIssuedAt(now)
    .setExpirationTime(now + TOKEN_TTL_SECONDS)
    .sign(encoder.encode(secret));
}

/** The subject of a valid token, or `null`. */
export async function readToken(
  token: string,
  secret: string,
): Promise<string | null> {
  try {
    const { payload } = await jwtVerify(token, encoder.encode(secret), {
      algorithms: ["HS256"],
    });
    return typeof payload.sub === "string" ? payload.sub : null;
  } catch {
    return null;
  }
}

/** The authenticated user id from an `Authorization: Bearer` header. */
export async function authenticate(
  request: Request,
  env: { JWT_SECRET: string },
): Promise<string | null> {
  const header = request.headers.get("authorization") ?? "";
  const [scheme, token] = header.split(" ");
  if (scheme?.toLowerCase() !== "bearer" || !token) {
    return null;
  }
  return readToken(token, env.JWT_SECRET);
}
