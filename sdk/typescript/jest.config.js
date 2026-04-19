// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/** @type {import('jest').Config} */
export default {
  preset: "ts-jest/presets/default-esm",
  testEnvironment: "node",
  extensionsToTreatAsEsm: [".ts"],
  moduleNameMapper: {
    "^(\\.{1,2}/.*)\\.js$": "$1",
  },
  testMatch: ["**/__tests__/**/*.test.ts"],
  transform: {
    "^.+\\.tsx?$": [
      "ts-jest",
      {
        useESM: true,
        tsconfig: {
          module: "ESNext",
          target: "ESNext",
          moduleResolution: "Bundler",
          esModuleInterop: true,
          strict: true,
        },
      },
    ],
  },
};
