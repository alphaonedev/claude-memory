// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * v0.6.4-007 — `requireProfile` SDK helper.
 *
 * NHI agents that depend on tools outside the v0.6.4 default `core`
 * profile call `requireProfile(client, "graph")` (or `admin` / `power` /
 * `full` / a custom `core,graph,archive` list) at startup. The helper
 * fetches `GET /api/v1/capabilities`, inspects the `families` block
 * added by the v0.6.4-006 capabilities extension, and throws a
 * structured {@link ProfileNotLoaded} error with an actionable hint
 * if any family the profile requires is not loaded.
 *
 * The helper is **purely additive**. Existing SDK consumers that don't
 * need profile-aware bootstrap remain untouched.
 *
 * @example
 * ```ts
 * import { AiMemoryClient, requireProfile, ProfileNotLoaded } from "ai-memory";
 *
 * const client = new AiMemoryClient({ baseUrl: "http://localhost:9077" });
 * try {
 *   await requireProfile(client, "graph");
 * } catch (e) {
 *   if (e instanceof ProfileNotLoaded) {
 *     console.error("Restart the MCP server with:", e.hint);
 *     process.exit(2);
 *   }
 *   throw e;
 * }
 * ```
 */

/**
 * Thin interface every `requireProfile` argument must satisfy. Lets
 * callers pass either a real `AiMemoryClient` or a mock with the
 * matching `.raw()` shape (used in unit tests).
 */
export interface CapabilitiesProbe {
  raw<T = unknown>(method: "GET", path: string): Promise<T>;
}

/**
 * Map of profile-name → families that must be loaded. Source-anchored
 * at `src/profile.rs::Profile::*`. Families not listed are not required;
 * `memory_capabilities` is always-on and never blocks acquisition.
 */
const PROFILE_FAMILY_REQUIREMENTS: Record<string, string[]> = {
  core: ["core"],
  graph: ["core", "graph"],
  admin: ["core", "lifecycle", "governance"],
  power: ["core", "power"],
  full: ["core", "lifecycle", "graph", "governance", "power", "meta", "archive", "other"],
};

const VALID_FAMILIES = [
  "core",
  "lifecycle",
  "graph",
  "governance",
  "power",
  "meta",
  "archive",
  "other",
];

/**
 * Thrown when a daemon does not load every family the requested
 * profile needs. The {@link hint} field contains a one-line CLI/env
 * snippet the operator can paste to restart the server with the
 * right profile.
 */
export class ProfileNotLoaded extends Error {
  readonly hint: string;
  readonly missing: string[];
  readonly requested: string;
  constructor(requested: string, missing: string[]) {
    const cliHint = `--profile ${requested}`;
    const envHint = `AI_MEMORY_PROFILE=${requested}`;
    const hint = `restart the ai-memory MCP server with \`${cliHint}\` (or set ${envHint}); missing families: ${missing.join(", ")}`;
    super(`profile '${requested}' not fully loaded — ${hint}`);
    this.name = "ProfileNotLoaded";
    this.hint = hint;
    this.missing = missing;
    this.requested = requested;
  }
}

/**
 * Resolve the family set required by `profile`. Accepts named profiles
 * (`core`, `graph`, `admin`, `power`, `full`) and comma-separated
 * custom lists (`core,graph,archive`).
 *
 * @internal Exposed for unit tests; consumers should call
 *           {@link requireProfile} instead.
 */
export function resolveRequiredFamilies(profile: string): string[] {
  const trimmed = profile.trim();
  if (trimmed === "") return ["core"];
  const named = PROFILE_FAMILY_REQUIREMENTS[trimmed];
  if (named !== undefined) return named;
  // Comma-list custom. Validate every token; surface unknown tokens
  // up front so a typo at startup is a deterministic error rather
  // than a "profile is loaded but tool calls still fail" mystery.
  const requested = new Set<string>(["core"]);
  for (const raw of trimmed.split(",")) {
    const tok = raw.trim();
    if (tok === "") continue;
    if (tok === "full") {
      return PROFILE_FAMILY_REQUIREMENTS.full;
    }
    if (PROFILE_FAMILY_REQUIREMENTS[tok] !== undefined) {
      for (const f of PROFILE_FAMILY_REQUIREMENTS[tok]) requested.add(f);
      continue;
    }
    if (!VALID_FAMILIES.includes(tok)) {
      throw new Error(
        `unknown profile or family '${tok}'. Valid: ${VALID_FAMILIES.join(", ")}, full`,
      );
    }
    requested.add(tok);
  }
  return [...requested];
}

interface FamilyRow {
  name: string;
  loaded: boolean;
}

interface CapabilitiesResponse {
  families?: {
    families: FamilyRow[];
  };
}

/**
 * Verify that the daemon at `client` has every family for `profile`
 * loaded. Throws {@link ProfileNotLoaded} if any are missing.
 *
 * If the daemon is pre-v0.6.4 (no `families` block in the
 * capabilities response), the helper falls back to a permissive
 * `no-op + warn` so existing programmatic users on legacy servers
 * don't see a regression. The warning is one console.warn line; we
 * deliberately do not introduce a logger dependency here.
 */
export async function requireProfile(
  client: CapabilitiesProbe,
  profile: string,
): Promise<void> {
  const required = resolveRequiredFamilies(profile);
  const caps = await client.raw<CapabilitiesResponse>(
    "GET",
    "/api/v1/capabilities",
  );
  const familiesBlock = caps?.families?.families;
  if (familiesBlock === undefined) {
    // Pre-v0.6.4 daemon — best-effort skip with a single warn line.
    // Operators upgrading the SDK before the daemon will see this and
    // know to upgrade the server.
    console.warn(
      "ai-memory SDK requireProfile: daemon predates v0.6.4; cannot verify profile. Skipping check.",
    );
    return;
  }
  const loaded = new Set(
    familiesBlock.filter((row) => row.loaded === true).map((row) => row.name),
  );
  // memory_capabilities is always-on; treat its family as available
  // for the limited purpose of "can the agent at least bootstrap".
  // We do NOT add other meta tools here — if the user asked for
  // `meta`, they want all five.
  const missing = required.filter((f) => !loaded.has(f));
  if (missing.length > 0) {
    throw new ProfileNotLoaded(profile, missing);
  }
}
