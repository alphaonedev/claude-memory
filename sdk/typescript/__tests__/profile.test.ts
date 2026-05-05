// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * v0.6.4-007 — unit tests for the `requireProfile` SDK helper.
 *
 * These tests use a hand-rolled mock that satisfies the
 * `CapabilitiesProbe` interface (only needs a `raw()` method) so we
 * never need a live daemon or a network round-trip. Every test runs
 * in CI without `AI_MEMORY_TEST_DAEMON=1`.
 */

import {
  ProfileNotLoaded,
  requireProfile,
  resolveRequiredFamilies,
  type CapabilitiesProbe,
} from "../src/profile.js";

interface FamilyRow {
  name: string;
  loaded: boolean;
}

function makeProbe(rows: FamilyRow[]): CapabilitiesProbe {
  return {
    async raw<T = unknown>(method: "GET", path: string): Promise<T> {
      expect(method).toBe("GET");
      expect(path).toBe("/api/v1/capabilities");
      return {
        families: { families: rows },
      } as unknown as T;
    },
  };
}

const ALL_FAMILIES_LOADED: FamilyRow[] = [
  { name: "core", loaded: true },
  { name: "lifecycle", loaded: true },
  { name: "graph", loaded: true },
  { name: "governance", loaded: true },
  { name: "power", loaded: true },
  { name: "meta", loaded: true },
  { name: "archive", loaded: true },
  { name: "other", loaded: true },
];

const ONLY_CORE_LOADED: FamilyRow[] = [
  { name: "core", loaded: true },
  { name: "lifecycle", loaded: false },
  { name: "graph", loaded: false },
  { name: "governance", loaded: false },
  { name: "power", loaded: false },
  { name: "meta", loaded: false },
  { name: "archive", loaded: false },
  { name: "other", loaded: false },
];

describe("resolveRequiredFamilies", () => {
  test("named profiles map to documented family sets", () => {
    expect(resolveRequiredFamilies("core")).toEqual(["core"]);
    expect(resolveRequiredFamilies("graph").sort()).toEqual(
      ["core", "graph"].sort(),
    );
    expect(resolveRequiredFamilies("admin").sort()).toEqual(
      ["core", "governance", "lifecycle"].sort(),
    );
    expect(resolveRequiredFamilies("power").sort()).toEqual(
      ["core", "power"].sort(),
    );
    expect(resolveRequiredFamilies("full")).toHaveLength(8);
  });

  test("empty input → core", () => {
    expect(resolveRequiredFamilies("")).toEqual(["core"]);
    expect(resolveRequiredFamilies("   ")).toEqual(["core"]);
  });

  test("comma-list dedupe", () => {
    const r = resolveRequiredFamilies("core,graph,core");
    expect(r.sort()).toEqual(["core", "graph"].sort());
  });

  test("comma-list with full subsumes", () => {
    expect(resolveRequiredFamilies("core,graph,full")).toHaveLength(8);
  });

  test("comma-list always-includes core", () => {
    const r = resolveRequiredFamilies("archive");
    expect(r).toContain("core");
    expect(r).toContain("archive");
  });

  test("unknown family throws with diagnostic", () => {
    expect(() => resolveRequiredFamilies("xyz")).toThrow(/unknown profile or family/);
  });
});

describe("requireProfile", () => {
  test("resolves cleanly when all required families are loaded", async () => {
    const probe = makeProbe(ALL_FAMILIES_LOADED);
    await expect(requireProfile(probe, "graph")).resolves.toBeUndefined();
    await expect(requireProfile(probe, "full")).resolves.toBeUndefined();
  });

  test("throws ProfileNotLoaded when graph family is missing", async () => {
    const probe = makeProbe(ONLY_CORE_LOADED);
    await expect(requireProfile(probe, "graph")).rejects.toBeInstanceOf(
      ProfileNotLoaded,
    );
  });

  test("error message includes actionable --profile hint", async () => {
    const probe = makeProbe(ONLY_CORE_LOADED);
    let thrown: ProfileNotLoaded | null = null;
    try {
      await requireProfile(probe, "graph");
    } catch (e) {
      thrown = e as ProfileNotLoaded;
    }
    expect(thrown).not.toBeNull();
    expect(thrown!.hint).toContain("--profile graph");
    expect(thrown!.hint).toContain("AI_MEMORY_PROFILE=graph");
    expect(thrown!.missing).toContain("graph");
    expect(thrown!.requested).toBe("graph");
  });

  test("core profile passes when only core is loaded", async () => {
    const probe = makeProbe(ONLY_CORE_LOADED);
    await expect(requireProfile(probe, "core")).resolves.toBeUndefined();
  });

  test("admin profile fails when lifecycle is missing", async () => {
    const probe = makeProbe([
      { name: "core", loaded: true },
      { name: "governance", loaded: true },
      { name: "lifecycle", loaded: false },
      { name: "graph", loaded: false },
      { name: "power", loaded: false },
      { name: "meta", loaded: false },
      { name: "archive", loaded: false },
      { name: "other", loaded: false },
    ]);
    await expect(requireProfile(probe, "admin")).rejects.toThrow(/lifecycle/);
  });

  test("pre-v0.6.4 daemon (no families block) — falls back to permissive warn", async () => {
    // Capture console.warn to assert the fallback path was taken.
    const originalWarn = console.warn;
    let warned = "";
    console.warn = (msg: string) => {
      warned = msg;
    };
    try {
      const probe: CapabilitiesProbe = {
        async raw<T = unknown>(): Promise<T> {
          // Legacy capabilities response — no `families` block.
          return { schema_version: "2", features: {} } as unknown as T;
        },
      };
      await expect(requireProfile(probe, "graph")).resolves.toBeUndefined();
      expect(warned).toContain("predates v0.6.4");
    } finally {
      console.warn = originalWarn;
    }
  });
});
