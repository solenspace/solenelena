// Mint an Elena HS256 JWT. Args via process.env:
//   ELENA_JWT_SECRET   HS256 secret (required)
//   TENANT_ID, USER_ID, WORKSPACE_ID  (required)
//   TIER       free|pro|team|enterprise (default pro)
//   EXP_OFFSET seconds from now until expiry (default 3600; negative = expired)
//   ISS, AUD   (default elena, elena-clients)
//
// Stdout: the signed token (no trailing newline).

const jwt = require("jsonwebtoken");
const { ulid } = require("ulid");

const secret = process.env.ELENA_JWT_SECRET;
if (!secret) { console.error("ELENA_JWT_SECRET required"); process.exit(2); }

const off = Number(process.env.EXP_OFFSET || 3600);

// All Elena IDs are ULIDs (26-char Crockford-Base32). UUIDs fail JWT
// deserialization with "malformed token" because the SessionId type
// expects a ULID-shaped string.
const claims = {
  tenant_id: process.env.TENANT_ID,
  user_id: process.env.USER_ID,
  workspace_id: process.env.WORKSPACE_ID,
  session_id: ulid(),
  tier: process.env.TIER || "pro",
  iss: process.env.ISS || "elena",
  aud: process.env.AUD || "elena-clients",
  exp: Math.floor(Date.now() / 1000) + off,
};

process.stdout.write(jwt.sign(claims, secret, { algorithm: "HS256" }));
