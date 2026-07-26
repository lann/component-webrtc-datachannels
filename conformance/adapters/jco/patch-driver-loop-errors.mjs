// Post-transpile patch: make the jco async driver loop's swallowed errors loud.
//
// jco's generated `_driverLoop` wraps the guest-event dispatch in a try/catch
// whose handler only calls `_debugLog` (a no-op unless JCO_DEBUG is set) and
// then exits the loop. A guest trap inside an event callback therefore
// vanishes: the export's promise never settles and the test times out with no
// diagnostic (see TODO E17). This script rewrites the catch handler to also
// `console.error` the error, so a swallowed guest crash names itself in the
// adapter output instead of surfacing as a bare timeout.
//
// Usage: node patch-driver-loop-errors.mjs <generated-js-file>
// Exits non-zero if the expected catch block is not found, so a jco upgrade
// that reshapes the generated code fails the transpile step visibly.

import { readFileSync, writeFileSync } from "node:fs";

const path = process.argv[2];
if (!path) {
  console.error("usage: node patch-driver-loop-errors.mjs <generated-js-file>");
  process.exit(2);
}

const marker = "_debugLog('[_driverLoop()] error during async driver loop', {";
const loud =
  "console.error('[jco _driverLoop] swallowed error during async driver loop " +
  "(guest task abandoned; an in-flight export will never settle):', err);\n      " +
  marker;

const src = readFileSync(path, "utf8");
const count = src.split(marker).length - 1;
if (count !== 1) {
  console.error(
    `patch-driver-loop-errors: expected exactly 1 driver-loop catch marker in ${path}, found ${count}; ` +
      "jco's generated code has changed — update this patch",
  );
  process.exit(1);
}
writeFileSync(path, src.replace(marker, loud));
console.error(`patch-driver-loop-errors: patched ${path}`);
